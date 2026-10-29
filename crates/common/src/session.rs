use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::subscription::Payer;

/// The transcript a CLI keeps for an agent's turns in one session: which
/// one, how many turns it has answered, when it last did, and the last
/// session message it has seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSlot {
    pub id: Uuid,
    pub turns: u64,
    pub last_reply_at: Option<DateTime<Utc>>,
    /// The id of the last message of this session the transcript holds.
    pub synced_through: Option<String>,
}

impl EngineSlot {
    /// A transcript no turn has answered in yet.
    pub fn fresh() -> Self {
        Self {
            id: Uuid::new_v4(),
            turns: 0,
            last_reply_at: None,
            synced_through: None,
        }
    }
}

// ─── Compaction Types ───

/// Auto-spill of large tabular tool outputs to the artifact store
///: outputs over the threshold that parse
/// as row tables are staged and replaced with a digest envelope before
/// they reach context, persistence, or traces. Global-only; hot-reloads
/// with the other runtime knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpillConfig {
    #[serde(default = "default_spill_enabled")]
    pub enabled: bool,
    #[serde(default = "default_spill_threshold_bytes")]
    pub threshold_bytes: usize,
    /// Tool-name prefixes whose outputs are CONTENT the model must read
    /// (recall, knowledge, transcripts) rather than data to transform —
    /// spilling those degrades answers into a re-query round, so they
    /// always pass through. Data adapters (linear/jira/github/metabase)
    /// are deliberately absent: their bulk list/search results are
    /// tabular record sets, so they spill like any other large table.
    /// The shape gate (`locate_rows`) still keeps single-record reads
    /// such as `issue_get` — a lone object, never an array — inline.
    #[serde(default = "default_spill_skip_tools")]
    pub skip_tools: Vec<String>,
}

fn default_spill_enabled() -> bool {
    true
}

fn default_spill_threshold_bytes() -> usize {
    32 * 1024
}

fn default_spill_skip_tools() -> Vec<String> {
    // Content namespaces only: outputs the model reads as an answer, not
    // record sets to transform. Data adapters (linear/jira/github/metabase)
    // are intentionally NOT here — their large list/search results are
    // exactly the tabular data the spill machinery exists to stage.
    // `tool.retrieve` is the recovery primitive itself: its result is already
    // a bounded answer, so re-staging it would hand the agent a fresh handle
    // instead of the content it asked for.
    [
        "memory",
        "knowledge",
        "sessions",
        "session",
        "notes",
        "context",
        "tool.retrieve",
    ]
    .map(str::to_string)
    .to_vec()
}

impl Default for SpillConfig {
    fn default() -> Self {
        Self {
            enabled: default_spill_enabled(),
            threshold_bytes: default_spill_threshold_bytes(),
            skip_tools: default_spill_skip_tools(),
        }
    }
}

/// Configuration for session compaction / context window management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Maximum context window size in tokens (default 200,000).
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: u32,
    /// Fraction of max tokens at which compaction triggers (default 0.8 = 80%).
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: f32,
    /// Number of recent messages to always keep during compaction (default 20).
    #[serde(default = "default_keep_recent_messages")]
    pub keep_recent_messages: usize,
    /// Whether to flush important context to memory files before compacting (default true).
    #[serde(default = "default_enable_memory_flush")]
    pub enable_memory_flush: bool,
    /// Whether to prune large tool outputs as a lighter-weight alternative (default true).
    #[serde(default = "default_prune_tool_outputs")]
    pub prune_tool_outputs: bool,
    /// Maximum character length for tool output before truncation (default 2000).
    #[serde(default = "default_prune_tool_output_max_chars")]
    pub prune_tool_output_max_chars: usize,
    /// Maximum character length for tool input before truncation (default 1024).
    /// Mirrors `prune_tool_output_max_chars`. Applied to consumed rounds only —
    /// the current round's input always stays full-fidelity.
    #[serde(default = "default_prune_tool_input_max_chars")]
    pub prune_tool_input_max_chars: usize,
    /// Maximum compaction iterations per cycle — safety valve (default 3).
    #[serde(default = "default_max_compaction_passes")]
    pub max_compaction_passes: u32,
    /// Minimum messages to compact — skip if fewer non-system messages would be
    /// replaced, to avoid degenerate summaries (default 5).
    #[serde(default = "default_min_messages_to_compact")]
    pub min_messages_to_compact: usize,
    /// Fraction of max tokens at which compaction MUST block synchronously
    /// (default 0.95 = 95%). Between `compaction_threshold` and this value,
    /// compaction is deferred to a background task after the agent responds.
    #[serde(default = "default_urgent_threshold")]
    pub urgent_compaction_threshold: f32,
    /// Fraction of the context window at which the mid-turn watchdog evicts
    /// stale in-flight tool outputs (default 0.60). Batched under pressure —
    /// rather than every round — so the message-cache prefix is invalidated
    /// once instead of per round. Strictly below
    /// `urgent_compaction_threshold`, giving eviction a chance to relieve
    /// pressure before summary compaction fires.
    #[serde(default = "default_soft_eviction_threshold")]
    pub soft_eviction_threshold: f32,
    /// Fraction of the compaction trigger at which a thread rolls over to a
    /// fresh session (default 0.70), strictly below 1.0 so rollover fires
    /// before compaction would. Absolute watermark =
    /// `max_context_tokens * compaction_threshold * rollover_warning_ratio`.
    #[serde(default = "default_rollover_warning_ratio")]
    pub rollover_warning_ratio: f32,
    /// Recent turns carried verbatim into a rolled-over session, on top of the
    /// carry-over summary (default 4).
    #[serde(default = "default_rollover_keep_recent")]
    pub rollover_keep_recent: usize,
    /// Model to use for compaction LLM calls (summarisation, memory flush).
    /// If None, uses the agent's default model. Set to a fast model like
    /// "claude-haiku-4-5-20251001" for faster compaction.
    #[serde(default = "default_compaction_model")]
    pub compaction_model: Option<String>,
    /// Custom system prompt for memory flush (what to persist before compaction).
    /// When None, uses the built-in default. Set per-agent for task-specific
    /// memory extraction (e.g. voice chats need conversation substance, not tool metadata).
    #[serde(default)]
    pub flush_prompt: Option<String>,
    /// Custom system prompt for conversation summarisation.
    /// When None, uses the built-in default. Set per-agent to control what
    /// the summary emphasises (e.g. emotional context for personal chats).
    #[serde(default)]
    pub summary_prompt: Option<String>,
    /// Hours of inactivity after which resuming a conversational-channel
    /// session rolls it over to a fresh successor with a dated carry-over,
    /// instead of replaying weeks-old messages as if current (default 24;
    /// 0 disables). Token-watermark rollover alone can't catch this: voice
    /// and chat turns accrue tokens far too slowly.
    #[serde(default = "default_stale_rollover_hours")]
    pub stale_rollover_hours: u64,
    /// Conversational message count at which a voice session rolls over
    /// regardless of token pressure (default 80; 0 disables). Voice turns are
    /// tiny, so a spoken thread degrades on message volume long before the
    /// token watermark is reachable.
    #[serde(default = "default_voice_rollover_max_messages")]
    pub voice_rollover_max_messages: usize,
    /// Conversational message count at which a non-voice session on a
    /// conversational channel (ios/web/terminal) rolls over (default 200;
    /// 0 disables). Chat turns are small, so a heavily-used text thread that
    /// never goes dormant would otherwise replay hundreds of messages per
    /// turn before reaching the token watermark. The session a scheduled job
    /// is pinned to rolls at the same count, over every message it replays.
    #[serde(default = "default_chat_rollover_max_messages")]
    pub chat_rollover_max_messages: usize,
}

/// Deployment-wide ceiling when an agent doesn't set its own.
///
/// DELIBERATELY still 200K even though the current Claude models hold 1M.
/// Raising this looks like the fix for "my agent compacts too early" and is
/// not, because not every path that reads it is model-aware: `CompactionEngine`
/// is built from the raw config, so a model whose real window is BELOW this
/// value would have its own trigger computed from this number and would decline
/// to compact until far past what it can actually hold. It is also a silent 5x
/// on per-turn input spend for every install that inherits it.
///
/// A deployment that wants the bigger window opts in explicitly, per agent or
/// in its global `[compaction]` block — where it is reviewable and where the
/// operator knows which model the agent actually runs.
fn default_max_context_tokens() -> u32 {
    200_000
}

/// Denominator for the coarse session-usage display when no agent config is in
/// scope. Deliberately NOT [`default_max_context_tokens`]: this drives a
/// user-visible "how full is this session" percentage and a 0.8 warning, and
/// following the raised ceiling would under-report every legacy-model session.
/// Callers that can reach the agent's resolved `CompactionConfig` should use
/// that instead — see `context_usage_for`.
const DISPLAY_CONTEXT_TOKENS_FALLBACK: u32 = 200_000;
fn default_compaction_threshold() -> f32 {
    0.8
}
fn default_keep_recent_messages() -> usize {
    20
}
fn default_enable_memory_flush() -> bool {
    true
}
fn default_prune_tool_outputs() -> bool {
    true
}
fn default_prune_tool_output_max_chars() -> usize {
    2000
}
fn default_prune_tool_input_max_chars() -> usize {
    1024
}

/// Storage-side caps for tool-use input. Looser than the in-context default —
/// disk is cheap, but unbounded inputs harm transcript browsing and risk
/// blowing the next-turn replay budget.
const STORAGE_INPUT_MAX_CHARS: usize = 4096;
const STORAGE_INPUT_FIELD_MAX_CHARS: usize = 1024;

/// Cap on the delivered text a tool row's content carries, keeping the row
/// bounded and its generated tsvector far below Postgres' 1 MiB limit.
const DELIVERED_TEXT_MAX_BYTES: usize = 32_768;

/// Session metadata key holding the rich blocks the speaking client renders.
const RICH_RENDERED_KEY: &str = "rich";

fn default_max_compaction_passes() -> u32 {
    3
}
fn default_min_messages_to_compact() -> usize {
    5
}
fn default_urgent_threshold() -> f32 {
    0.95
}
fn default_soft_eviction_threshold() -> f32 {
    0.60
}
fn default_rollover_warning_ratio() -> f32 {
    0.70
}
fn default_rollover_keep_recent() -> usize {
    4
}
fn default_compaction_model() -> Option<String> {
    None // Use the active model for compaction — no hardcoded provider dependency
}
fn default_stale_rollover_hours() -> u64 {
    24
}
fn default_voice_rollover_max_messages() -> usize {
    80
}
fn default_chat_rollover_max_messages() -> usize {
    200
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            // Via the fn, not a literal — a `Default` that silently disagrees
            // with the serde default is how the two drift apart.
            max_context_tokens: default_max_context_tokens(),
            compaction_threshold: 0.8,
            keep_recent_messages: 20,
            enable_memory_flush: true,
            prune_tool_outputs: true,
            prune_tool_output_max_chars: 2000,
            prune_tool_input_max_chars: default_prune_tool_input_max_chars(),
            max_compaction_passes: default_max_compaction_passes(),
            min_messages_to_compact: default_min_messages_to_compact(),
            urgent_compaction_threshold: default_urgent_threshold(),
            soft_eviction_threshold: default_soft_eviction_threshold(),
            rollover_warning_ratio: default_rollover_warning_ratio(),
            rollover_keep_recent: default_rollover_keep_recent(),
            compaction_model: None,
            flush_prompt: None,
            summary_prompt: None,
            stale_rollover_hours: default_stale_rollover_hours(),
            voice_rollover_max_messages: default_voice_rollover_max_messages(),
            chat_rollover_max_messages: default_chat_rollover_max_messages(),
        }
    }
}

impl CompactionConfig {
    /// The token threshold at which compaction should trigger.
    pub fn trigger_threshold(&self) -> u32 {
        (self.max_context_tokens as f32 * self.compaction_threshold) as u32
    }

    /// The token threshold at which compaction MUST run synchronously (cannot be deferred).
    pub fn urgent_threshold(&self) -> u32 {
        (self.max_context_tokens as f32 * self.urgent_compaction_threshold) as u32
    }
}

/// The non-message portion of every LLM request: the system prompt, the
/// serialised tool schemas, and the tokens reserved for the model's response.
/// Compaction thresholds must account for these — message tokens alone
/// understate true context usage by the size of this fixed overhead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReservedContext {
    pub system_prompt_tokens: u32,
    pub tool_schema_tokens: u32,
    pub response_reservation_tokens: u32,
}

impl ReservedContext {
    pub fn total(self) -> u32 {
        self.system_prompt_tokens
            .saturating_add(self.tool_schema_tokens)
            .saturating_add(self.response_reservation_tokens)
    }

    /// Add prompt content that is sent this turn outside the system string —
    /// the hub's per-turn context block leading the task message. The
    /// pre-turn estimate counts it here because it is prompt floor for this
    /// turn; the mid-turn watchdog sees it inside the messages and must not
    /// add it again.
    pub fn with_prompt_tokens(self, tokens: u32) -> Self {
        Self {
            system_prompt_tokens: self.system_prompt_tokens.saturating_add(tokens),
            ..self
        }
    }
}

/// Metadata recorded when a compaction event occurs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionEvent {
    /// When the compaction happened.
    pub timestamp: DateTime<Utc>,
    /// Number of messages that were compacted (replaced by the summary).
    pub messages_compacted: usize,
    /// Estimated token count before compaction.
    pub tokens_before: u64,
    /// Estimated token count after compaction.
    pub tokens_after: u64,
    /// What type of compaction was performed.
    pub kind: CompactionKind,
}

/// The type of compaction that was performed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionKind {
    /// Full summarisation — old messages replaced by an LLM-generated summary.
    Summarisation,
    /// Lightweight pruning — large tool outputs truncated.
    Pruning,
    /// Manual compaction triggered by a user/command.
    Manual,
    /// Emergency compaction — aggressive pruning when normal compaction can't reduce enough.
    Emergency,
}

/// Result of a selective tool output eviction operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvictionResult {
    /// Number of tool outputs that were evicted.
    pub evicted: usize,
    /// Number of tool outputs that were kept (protected by keep_last, min_chars, or filters).
    pub kept: usize,
    /// Estimated tokens freed by the eviction.
    pub tokens_freed: u64,
}

// ─── Transcript Types ───

/// A single entry in a session transcript (rich JSONL line).
///
/// Unlike `SessionMessage` (which stores the minimal info for conversation replay),
/// a `TranscriptEntry` captures everything needed for full observability:
/// tool metadata, token usage per turn, timing, and structured role information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    /// Unique entry identifier.
    pub id: String,
    /// Session this entry belongs to.
    pub session_id: String,
    /// When this entry was recorded.
    pub timestamp: DateTime<Utc>,
    /// Role of the message author.
    pub role: MessageRole,
    /// Text content of the message.
    pub content: String,
    /// Tool call metadata (present for tool-related entries).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_metadata: Option<ToolMetadata>,
    /// Token usage for this turn (present for LLM responses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TurnTokenUsage>,
    /// The message's place in a Desk conversation, as
    /// [`SessionMessage::desk`] has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desk: Option<crate::channels::desk::DeskMark>,
}

/// Metadata about a tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMetadata {
    /// Tool name (e.g., "filesystem.read").
    pub name: String,
    /// Tool input parameters.
    pub input: serde_json::Value,
    /// Tool output.
    pub output: serde_json::Value,
    /// Execution time in milliseconds.
    pub duration_ms: u64,
    /// Whether the tool call succeeded.
    pub success: bool,
}

/// Token usage for a single turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnTokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// A page of archived messages for paginated transcript display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivePage {
    pub messages: Vec<TranscriptEntry>,
    pub has_more: bool,
}

/// A full session export — the complete session as a single JSON document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionExport {
    /// Session metadata.
    pub id: String,
    pub label: Option<String>,
    pub channel: String,
    pub owner: String,
    pub agent_id: String,
    pub kind: SessionKind,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub total_tokens: u64,
    /// Per-session model override, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
    /// All transcript entries in chronological order.
    pub transcript: Vec<TranscriptEntry>,
    /// Session statistics.
    pub stats: SessionStats,
}

/// Aggregated statistics about a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStats {
    /// Total number of messages.
    pub message_count: usize,
    /// Messages broken down by role.
    pub messages_by_role: HashMap<String, usize>,
    /// Token breakdown by role.
    pub tokens_by_role: HashMap<String, u64>,
    /// Total tokens across all turns.
    pub total_tokens: u64,
    /// Total input tokens from LLM calls.
    pub total_input_tokens: u64,
    /// Total output tokens from LLM calls.
    pub total_output_tokens: u64,
    /// Summary of tool calls.
    pub tool_calls: ToolCallSummary,
    /// Session duration (first message to last).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<i64>,
}

/// Aggregated tool call statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallSummary {
    /// Total number of tool invocations.
    pub total: usize,
    /// Number of successful invocations.
    pub succeeded: usize,
    /// Number of failed invocations.
    pub failed: usize,
    /// Total execution time across all tool calls.
    pub total_duration_ms: u64,
    /// Tool calls broken down by tool name.
    pub by_tool: HashMap<String, ToolStats>,
}

/// Per-tool statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolStats {
    pub count: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub total_duration_ms: u64,
}

/// A conversation session between a user and an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique session identifier.
    pub id: String,
    /// Human-readable label (e.g., "slack-dm-kings", "clawdy-main").
    pub label: Option<String>,
    /// Channel this session belongs to (slack, clawdy, cli, etc.).
    pub channel: String,
    /// User or channel ID that owns this session.
    pub owner: String,
    /// Agent ID handling this session.
    pub agent_id: String,
    /// Session kind: main, isolated, subagent.
    pub kind: SessionKind,
    /// Conversational context type (DM, group, thread).
    #[serde(default)]
    pub context_type: Option<SessionContextType>,
    /// Conversation messages.
    pub messages: Vec<SessionMessage>,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// Last activity timestamp.
    pub updated_at: DateTime<Utc>,
    /// Session metadata.
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// Total tokens used in this session.
    pub total_tokens: u64,
    /// Compaction events that have occurred in this session.
    #[serde(default)]
    pub compaction_history: Vec<CompactionEvent>,
    /// Per-session model override. When set, takes priority over the agent-wide
    /// active_model override and config default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
    /// Shell marker for the lazy-load store: `Some(N)` means this session
    /// is meta-only (`messages` deliberately empty to save heap) and the
    /// disk-known count is `N`. `None` means hydrated — `messages.len()`
    /// is authoritative. Any code that touches `messages` (hydration or
    /// mutation) must reset this to `None`, so the save path can rely on
    /// `Some(_)` to mean "this is a shell, don't overwrite the JSONL".
    #[serde(skip, default)]
    pub cached_message_count: Option<usize>,
    /// Optimistic-concurrency revision the session was loaded at, for the
    /// Postgres backend's append-preserving `save`. Stamped from the row by
    /// `get`/`find`/`get_or_create`; `None` for a freshly minted session or
    /// any store (e.g. [`JsonlSessionStore`]) that doesn't version rows.
    /// Mirrors `cached_message_count`: never serialised, never persisted.
    #[serde(skip, default)]
    pub loaded_revision: Option<i64>,
}

/// Session types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// Main session (direct chat with user).
    Main,
    /// Isolated session (channel-specific).
    Isolated,
    /// Sub-agent session (spawned for a task).
    SubAgent,
}

/// The conversational context type of a session.
///
/// This describes the "shape" of the conversation (DM, group channel, thread)
/// and is used by session reset policies to apply different rules per context.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionContextType {
    /// Direct message (1:1 with the bot).
    Dm,
    /// Group channel (multi-user channel where bot participates).
    Group,
    /// Thread within a channel.
    Thread,
}

impl std::fmt::Display for SessionContextType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dm => write!(f, "dm"),
            Self::Group => write!(f, "group"),
            Self::Thread => write!(f, "thread"),
        }
    }
}

/// Is this MIME type one whose bytes are meant to reach the model as readable
/// text rather than base64?
///
/// This is the single source of truth for the [`SessionAttachment::data`]
/// encoding contract. Every producer encodes by it and every consumer decodes
/// by it; two copies that drift is exactly how a 91KB YAML reached a model as
/// 100,000 characters of base64.
pub fn is_text_like_mime(mime: &str) -> bool {
    if mime.starts_with("text/") {
        return true;
    }
    matches!(
        mime,
        "application/json"
            | "application/xml"
            | "application/yaml"
            | "application/x-yaml"
            | "application/toml"
            | "application/javascript"
            | "application/x-javascript"
            | "application/typescript"
            | "application/x-sh"
            | "application/sql"
            | "application/graphql"
            | "application/x-httpd-php"
            | "application/xhtml+xml"
            | "application/ld+json"
            | "application/x-ndjson"
    )
}

/// A file or image attachment stored with a session message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAttachment {
    /// Original filename.
    pub filename: String,
    /// MIME type (e.g. "image/png", "text/plain").
    pub mime_type: String,
    /// File data — raw UTF-8 for text-like types (see [`is_text_like_mime`]),
    /// base64 for everything else. Build it with [`SessionAttachment::from_bytes`]
    /// rather than encoding by hand. Empty when `artifact_id` is set.
    #[serde(default)]
    pub data: String,
    /// When set, the payload lives in the shared artifact store under this id
    /// and `data` is empty. Producers use this for large files so the bytes
    /// never ride the WebSocket/NATS message or the persisted session row.
    /// Build with [`SessionAttachment::from_artifact`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
}

impl SessionAttachment {
    /// Build a by-reference attachment: the payload was already written to
    /// the artifact store and consumers resolve it from there.
    pub fn from_artifact(
        filename: impl Into<String>,
        mime_type: impl Into<String>,
        artifact_id: impl Into<String>,
    ) -> Self {
        Self {
            filename: filename.into(),
            mime_type: mime_type.into(),
            data: String::new(),
            artifact_id: Some(artifact_id.into()),
        }
    }

    /// Build an attachment from raw bytes, applying the `data` encoding
    /// contract: text-like MIME types are decoded to UTF-8 so the model can
    /// read them directly; everything else is base64.
    ///
    /// Hand-rolling this is a bug magnet — a caller that always base64-encodes
    /// still routes through the text branch downstream (which inlines `data`
    /// verbatim) and the model receives base64 labelled as file contents.
    pub fn from_bytes(
        filename: impl Into<String>,
        mime_type: impl Into<String>,
        bytes: &[u8],
    ) -> Self {
        use base64::Engine as _;
        let mime_type = mime_type.into();
        let data = if is_text_like_mime(&mime_type) {
            String::from_utf8_lossy(bytes).into_owned()
        } else {
            base64::engine::general_purpose::STANDARD.encode(bytes)
        };
        Self {
            filename: filename.into(),
            mime_type,
            data,
            artifact_id: None,
        }
    }
}

/// A message in a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessage {
    /// Message ID.
    pub id: String,
    /// Role: user, assistant, system, tool.
    pub role: MessageRole,
    /// Text content.
    pub content: String,
    /// Tool use/result data (if applicable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_data: Option<serde_json::Value>,
    /// Timestamp.
    pub timestamp: DateTime<Utc>,
    /// Token count for this message.
    pub tokens: u32,
    /// External message ID (e.g., Slack message ts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// File/image attachments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<SessionAttachment>>,
    /// Trust classification at the time this message was received. Drives
    /// prompt-injection wrapping at LLM-assembly time. `None` on rows
    /// persisted before this field existed; treated as Owner-equivalent
    /// (no wrap) so legacy transcripts replay unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_level: Option<crate::trust::TrustLevel>,
    /// Identity attributed to this message in a shared (multi-human) session:
    /// the human who sent a user message, or the human whose authorization a
    /// tool ran under. `None` for assistant/system messages and for
    /// single-owner (DM/private) sessions, so DMs and pre-existing transcripts
    /// replay unchanged. Drives per-speaker attribution and the leak-safe tool
    /// filter when a shared session replays to a different speaker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// What the message is in a Desk conversation: the room and thread it
    /// belongs to, who said it, and why it was delivered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desk: Option<crate::channels::desk::DeskMark>,
}

const COMPACTION_SUMMARY_PREFIX: &str = "[Compaction Summary";

/// Sentinel prefix for the `owner` of a shared (multi-human) conversation. A
/// shared session is keyed by thread rather than by a single user, so its
/// `owner` is `shared:{thread_root}` — a value no real identity or device id
/// can equal, which keeps it admin-only on the owner-scoped read path.
pub const SHARED_OWNER_PREFIX: &str = "shared:";

impl SessionMessage {
    /// The row recording one tool call: its input shrunk to the storage cap,
    /// its output, and how it went.
    pub fn tool(
        tool_name: &str,
        input: &serde_json::Value,
        output: &serde_json::Value,
        success: bool,
        duration_ms: u64,
        error: Option<&str>,
    ) -> Self {
        let mut input_to_store = input.clone();
        crate::tool_input::shrink_tool_input(
            &mut input_to_store,
            STORAGE_INPUT_FIELD_MAX_CHARS,
            STORAGE_INPUT_MAX_CHARS,
        );
        let mut tool_data = serde_json::json!({
            "name": tool_name,
            "input": input_to_store,
            "output": output,
            "duration_ms": duration_ms,
            "success": success,
        });
        if let Some(err) = error.filter(|e| !e.is_empty()) {
            tool_data["error"] = serde_json::Value::String(err.to_string());
        }
        let content = if success {
            let headline = format!("Tool '{}' completed in {}ms", tool_name, duration_ms);
            match crate::delivered_text::delivered_text(tool_name, input) {
                Some(text) => format!(
                    "{headline}\n\n{}",
                    crate::text::truncate_with_ellipsis(
                        &crate::redact::redact_secrets(&text),
                        DELIVERED_TEXT_MAX_BYTES,
                    )
                ),
                None => headline,
            }
        } else {
            format!("Tool '{}' failed after {}ms", tool_name, duration_ms)
        };
        Self {
            id: Uuid::new_v4().to_string(),
            role: MessageRole::Tool,
            content,
            tool_data: Some(tool_data),
            timestamp: Utc::now(),
            tokens: 0,
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
            desk: None,
        }
    }

    /// The first line of `content`: a tool row's account of how the call
    /// went, without the text it delivered.
    pub fn headline(&self) -> &str {
        self.content.lines().next().unwrap_or_default()
    }

    pub fn is_compaction_summary(&self) -> bool {
        self.role == MessageRole::System && self.content.starts_with(COMPACTION_SUMMARY_PREFIX)
    }
}

/// Message roles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

impl Session {
    /// Create a new session.
    pub fn new(channel: &str, owner: &str, agent_id: &str) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            label: None,
            channel: channel.to_string(),
            owner: owner.to_string(),
            agent_id: agent_id.to_string(),
            kind: SessionKind::Main,
            context_type: None,
            messages: Vec::new(),
            created_at: now,
            updated_at: now,
            metadata: serde_json::Value::Null,
            total_tokens: 0,
            compaction_history: Vec::new(),
            model_override: None,
            cached_message_count: None,
            loaded_revision: None,
        }
    }

    fn metadata_object_mut(&mut self) -> &mut serde_json::Map<String, serde_json::Value> {
        if !self.metadata.is_object() {
            self.metadata = serde_json::Value::Object(serde_json::Map::new());
        }
        self.metadata
            .as_object_mut()
            .expect("metadata was just ensured to be an object")
    }

    /// Whether this session has been sealed by a thread rollover. A sealed
    /// session is never returned by `get_or_create` — the thread's active
    /// session is the newest unsealed member sharing its `(channel, owner)`.
    pub fn is_sealed(&self) -> bool {
        self.metadata
            .get("sealed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Seal this session so it drops out of active selection. Touches only
    /// metadata, never `messages`, so it is safe to call on a lazy shell.
    pub fn seal(&mut self) {
        self.metadata_object_mut()
            .insert("sealed".into(), serde_json::json!(true));
        self.updated_at = Utc::now();
    }

    /// Monotonic position within the thread; rollover mints `seq + 1`.
    pub fn seq(&self) -> u64 {
        self.metadata
            .get("seq")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    }

    pub fn set_seq(&mut self, seq: u64) {
        self.metadata_object_mut()
            .insert("seq".into(), serde_json::json!(seq));
    }

    /// The session this one succeeded, if it was minted by a rollover.
    pub fn prev_session_id(&self) -> Option<&str> {
        self.metadata
            .get("prev_session_id")
            .and_then(|v| v.as_str())
    }

    pub fn set_prev_session_id(&mut self, id: &str) {
        self.metadata_object_mut()
            .insert("prev_session_id".into(), serde_json::json!(id));
    }

    /// The thread's next position: same conversation identity and label,
    /// `seq + 1`, nothing carried. A rollover additionally records the
    /// carry-over with [`Self::set_prev_session_id`].
    pub fn successor(&self) -> Session {
        let mut next = Session::new(&self.channel, &self.owner, &self.agent_id);
        next.label = self.label.clone();
        next.set_seq(self.seq().saturating_add(1));
        next
    }

    /// Whether this is a shared multi-human conversation, keyed by thread under
    /// the [`SHARED_OWNER_PREFIX`] sentinel rather than by a single user. Drives
    /// leak-safe replay and summary scoping.
    pub fn is_shared_conversation(&self) -> bool {
        self.owner.starts_with(SHARED_OWNER_PREFIX)
    }

    /// Attribute the User and Tool messages from index `from` onward to `actor`,
    /// the human a shared-session turn ran as. Assistant and System messages are
    /// left unattributed — the agent, and shared system text, belong to no single
    /// human. The turn path calls this once per shared-session turn so later
    /// replays can label each speaker and drop other speakers' raw tool output.
    pub fn attribute_messages_from(&mut self, from: usize, actor: &str) {
        for msg in self.messages.iter_mut().skip(from) {
            if matches!(msg.role, MessageRole::User | MessageRole::Tool) {
                msg.actor = Some(actor.to_string());
            }
        }
    }

    /// Progress through an active [`PlaybookProgress`], if a playbook is running
    /// on this session. Hung off metadata so it persists and follows rollover.
    pub fn playbook(&self) -> Option<crate::playbook_state::PlaybookProgress> {
        self.metadata
            .get("playbook")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    pub fn set_playbook(&mut self, progress: &crate::playbook_state::PlaybookProgress) {
        self.metadata_object_mut()
            .insert("playbook".into(), serde_json::json!(progress));
        self.updated_at = Utc::now();
    }

    /// Clear any active playbook (on Exit or completion), leaving the rest of
    /// the session — and its accumulated context — untouched.
    pub fn clear_playbook(&mut self) {
        self.metadata_object_mut().remove("playbook");
        self.updated_at = Utc::now();
    }

    /// The previous turn's replayed-history fingerprint, kept opaque here: the
    /// hub owns its shape.
    pub fn replay_prefix(&self) -> Option<&serde_json::Value> {
        self.metadata.get("replay_prefix")
    }

    pub fn set_replay_prefix(&mut self, prefix: serde_json::Value) {
        self.metadata_object_mut()
            .insert("replay_prefix".into(), prefix);
    }

    /// The previous turn's stable-system-prompt fingerprint, opaque here like
    /// [`Self::replay_prefix`].
    pub fn system_prefix(&self) -> Option<&serde_json::Value> {
        self.metadata.get("system_prefix")
    }

    pub fn set_system_prefix(&mut self, prefix: serde_json::Value) {
        self.metadata_object_mut()
            .insert("system_prefix".into(), prefix);
    }

    /// Per-session active environment overrides for gated MCP workspaces,
    /// `workspace -> env`. Set by a human `cmd: use <ws> <env>`; read each turn
    /// into the `ACTIVE_MCP_ENVS` task-local to decide which workspace member
    /// is reachable. Empty (the default) means each workspace falls back to its
    /// configured default env (or dormant). Persisted + follows rollover so an
    /// activation survives across turns within the session.
    pub fn active_mcp_envs(&self) -> std::collections::HashMap<String, String> {
        self.metadata
            .get("mcp_active_envs")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Point this session's `workspace` at `env` (a `cmd: use` activation).
    pub fn set_active_mcp_env(&mut self, workspace: &str, env: &str) {
        let obj = self.metadata_object_mut();
        let map = obj
            .entry("mcp_active_envs")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if !map.is_object() {
            *map = serde_json::Value::Object(serde_json::Map::new());
        }
        if let Some(m) = map.as_object_mut() {
            m.insert(workspace.to_string(), serde_json::json!(env));
        }
        self.updated_at = Utc::now();
    }

    /// Clear this session's override for `workspace`, reverting it to the
    /// configured default env (`cmd: use <ws> default`).
    pub fn clear_active_mcp_env(&mut self, workspace: &str) {
        if let Some(map) = self
            .metadata
            .get_mut("mcp_active_envs")
            .and_then(|v| v.as_object_mut())
        {
            map.remove(workspace);
        }
        self.updated_at = Utc::now();
    }

    /// This session's active environment name (a `cmd: env use` override), if
    /// set. Persisted + follows rollover; when absent the turn falls back to the
    /// global default environment. See `[environments]`.
    pub fn active_environment(&self) -> Option<String> {
        self.metadata
            .get("active_environment")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// Point this session at environment `name` (a `cmd: env use` activation).
    pub fn set_active_environment(&mut self, name: &str) {
        self.metadata_object_mut()
            .insert("active_environment".to_string(), serde_json::json!(name));
        self.updated_at = Utc::now();
    }

    /// Clear this session's environment override, reverting to the global
    /// default (`cmd: env use default`).
    pub fn clear_active_environment(&mut self) {
        if let Some(obj) = self.metadata.as_object_mut() {
            obj.remove("active_environment");
        }
        self.updated_at = Utc::now();
    }

    /// The tools an automated turn that opened this session declared, kept so
    /// a turn resumed by session id is offered the same surface.
    pub fn tool_whitelist(&self) -> Option<Vec<String>> {
        self.metadata
            .get("tool_whitelist")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    pub fn set_tool_whitelist(&mut self, tools: &[String]) {
        self.metadata_object_mut()
            .insert("tool_whitelist".into(), serde_json::json!(tools));
    }

    /// True between a rollover's sync seal+mint and its async carry-over
    /// seeding — guards against rolling a successor again before it is seeded.
    pub fn rollover_pending(&self) -> bool {
        self.metadata
            .get("rollover_pending")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub fn set_rollover_pending(&mut self, pending: bool) {
        self.metadata_object_mut()
            .insert("rollover_pending".into(), serde_json::json!(pending));
    }

    /// Whether this session is in voice mode (the iOS client set `voice_mode`).
    /// Drives voice-tailored summarisation/carry-over so spoken conversations
    /// preserve their conversational substance, not just task state.
    pub fn is_voice(&self) -> bool {
        self.metadata
            .get("voice_mode")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Whether the turn now arriving came in over voice, as opposed to
    /// [`Self::is_voice`], which says the *session* has been spoken to.
    ///
    /// The two differ because iOS pins text and speech to one eternal session
    /// and only sends `voice_mode` while its input mode is voice: `voice_mode`
    /// latches on and never clears, so it reads "this is a voice conversation"
    /// (what the rollover caps, the code-context gate and the digest want),
    /// while the gateway rewrites `voice_turn` on every inbound message so it
    /// reads "this message was spoken" (what model selection wants — typing
    /// after speaking should not inherit the voice model).
    pub fn is_voice_turn(&self) -> bool {
        self.metadata
            .get("voice_turn")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// The rich blocks the client now speaking on this session renders, as
    /// it declared them. Rewritten by the gateway on every inbound message,
    /// like [`Self::is_voice_turn`], since one session can be reached from
    /// more than one client.
    pub fn rendered_rich(&self) -> crate::rich::RichFeatures {
        crate::rich::RichFeatures::from_names(
            self.metadata
                .get(RICH_RENDERED_KEY)
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str()),
        )
    }

    /// Record what the client now speaking renders; `false` when that is
    /// what the session already held.
    pub fn set_rendered_rich(&mut self, rendered: crate::rich::RichFeatures) -> bool {
        if self.rendered_rich() == rendered {
            return false;
        }
        let metadata = self.metadata_object_mut();
        if rendered.is_empty() {
            metadata.remove(RICH_RENDERED_KEY);
        } else {
            metadata.insert(
                RICH_RENDERED_KEY.to_string(),
                serde_json::json!(rendered.names()),
            );
        }
        true
    }

    /// Copy the client-facing conversation-mode keys (voice mode, TTS
    /// provider, the rich blocks rendered) from `from` — a rollover
    /// successor must keep speaking the way its predecessor did. Selective
    /// by design: seq/prev/rollover state also lives in metadata and must
    /// not transfer. `voice_turn` rides along so a rollover landing mid-turn
    /// doesn't strand that turn's mode.
    pub fn adopt_conversation_mode(&mut self, from: &Session) {
        for key in [
            "voice_mode",
            "voice_turn",
            "tts_provider",
            RICH_RENDERED_KEY,
        ] {
            if let Some(v) = from.metadata.get(key) {
                self.metadata_object_mut()
                    .insert(key.to_string(), v.clone());
            }
        }
    }

    /// The Claude Code session UUID stored in metadata, if any.
    pub fn claude_session_uuid(&self) -> Option<&str> {
        self.metadata
            .get("claude_session_uuid")
            .and_then(|v| v.as_str())
    }

    /// The CLI transcript an agent's turns on one engine continue, kept at
    /// `key`; `None` when there is none, or what is kept there is not one.
    pub fn engine_slot(&self, key: &str) -> Option<EngineSlot> {
        self.metadata
            .get(key)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }

    /// Keep `slot` at `key`. Caller saves the session.
    pub fn set_engine_slot(&mut self, key: &str, slot: &EngineSlot) {
        if let Ok(value) = serde_json::to_value(slot) {
            self.metadata_object_mut().insert(key.to_string(), value);
        }
    }

    /// Return the existing Claude Code session UUID or generate a fresh one and persist it.
    /// Caller is responsible for saving the session after this call so the UUID survives restarts.
    pub fn get_or_init_claude_uuid(&mut self) -> String {
        if let Some(existing) = self.claude_session_uuid() {
            return existing.to_string();
        }
        let uuid = Uuid::new_v4().to_string();
        self.set_claude_uuid(&uuid);
        uuid
    }

    /// Make `uuid` the Claude Code session UUID.
    pub fn set_claude_uuid(&mut self, uuid: &str) {
        self.metadata_object_mut().insert(
            "claude_session_uuid".to_string(),
            serde_json::Value::String(uuid.to_string()),
        );
    }

    /// Drop the persisted Claude Code session UUID. Used after Claude Code reports
    /// `No conversation found with session ID` (the on-disk transcript is gone, so
    /// resuming will fail forever) — the next `get_or_init_claude_uuid` mints a
    /// fresh UUID and the session starts over without prior context. Also resets
    /// the per-UUID rotation counters so the new UUID starts from zero.
    pub fn clear_claude_session_uuid(&mut self) {
        let obj = self.metadata_object_mut();
        obj.remove("claude_session_uuid");
        obj.remove("claude_chat_turn_count");
        obj.remove("claude_chat_last_reply_at");
    }

    /// The agent-driver Claude Code session UUID for one project. Stored in a
    /// slot distinct from [`claude_session_uuid`] so the `coding.session` tool
    /// (agent as driver) and the `/cc` slash command (human as driver) never
    /// clobber each other's UUID or rotation state when they share a thread's
    /// session — and keyed **per project**: the worktree lives under
    /// `{project}/work/session/{uuid}`, so a single shared slot meant any
    /// project switch within a thread looked like a missing worktree, cold-
    /// starting Claude Code (and cleaning up the other project's state) on
    /// every alternation.
    pub fn driver_claude_session_uuid(&self, project: &str) -> Option<&str> {
        self.metadata
            .get(format!("driver_claude_session_uuid:{project}"))
            .and_then(|v| v.as_str())
    }

    /// Return the existing driver Claude Code session UUID for `project` or
    /// mint and persist a fresh one. Caller is responsible for saving the
    /// session afterwards so the UUID survives restarts and the next
    /// `coding.session` call resumes the same on-disk transcript via
    /// `--resume`.
    pub fn get_or_init_driver_claude_uuid(&mut self, project: &str) -> String {
        if let Some(existing) = self.driver_claude_session_uuid(project) {
            return existing.to_string();
        }
        let uuid = Uuid::new_v4().to_string();
        self.set_driver_claude_uuid(project, &uuid);
        uuid
    }

    /// Make `uuid` the driver Claude Code session UUID for `project`.
    pub fn set_driver_claude_uuid(&mut self, project: &str, uuid: &str) {
        let obj = self.metadata_object_mut();
        // Drop pre-project-keyed legacy slots: adopting one would be exactly
        // the cross-project ambiguity this keying removes, so the first call
        // per project cold-starts once and the stale keys stop lingering.
        obj.remove("driver_claude_session_uuid");
        obj.remove("driver_chat_turn_count");
        obj.remove("driver_chat_last_reply_at");
        obj.insert(
            format!("driver_claude_session_uuid:{project}"),
            serde_json::Value::String(uuid.to_string()),
        );
    }

    /// Drop the persisted driver Claude Code session UUID for `project`. Used
    /// after Claude Code reports `No conversation found with session ID` (the
    /// on-disk transcript is gone, e.g. a container redeploy wiped it) — the
    /// next `get_or_init_driver_claude_uuid` mints a fresh UUID and the driver
    /// session starts over without prior context.
    pub fn clear_driver_claude_session_uuid(&mut self, project: &str) {
        let obj = self.metadata_object_mut();
        obj.remove(&format!("driver_claude_session_uuid:{project}"));
        obj.remove(&format!("driver_chat_turn_count:{project}"));
        obj.remove(&format!("driver_chat_last_reply_at:{project}"));
    }

    /// Completed `coding.session` rounds under `project`'s current driver
    /// UUID; resets to zero when that UUID is cleared (rotation or recovery).
    pub fn driver_chat_turn_count(&self, project: &str) -> u64 {
        self.metadata
            .get(format!("driver_chat_turn_count:{project}"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    }

    /// Timestamp of the most recent completed driver round for `project`, or
    /// `None`.
    pub fn driver_chat_last_reply_at(&self, project: &str) -> Option<DateTime<Utc>> {
        self.metadata
            .get(format!("driver_chat_last_reply_at:{project}"))
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
    }

    /// Record that one driver round completed for `project`: bump the counter
    /// and stamp `now`. Caller saves the session.
    pub fn record_driver_chat_turn(&mut self, now: DateTime<Utc>, project: &str) {
        let next = self.driver_chat_turn_count(project).saturating_add(1);
        let obj = self.metadata_object_mut();
        obj.insert(
            format!("driver_chat_turn_count:{project}"),
            serde_json::Value::Number(next.into()),
        );
        obj.insert(
            format!("driver_chat_last_reply_at:{project}"),
            serde_json::Value::String(now.to_rfc3339()),
        );
    }

    /// Number of completed Claude-Code chat turns under the current UUID. Resets
    /// to zero whenever the UUID is cleared (rotation or recovery).
    pub fn claude_chat_turn_count(&self) -> u64 {
        self.metadata
            .get("claude_chat_turn_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    }

    /// Wall-clock timestamp of the most recent successful chat turn under the
    /// current UUID, or `None` if no turn has completed yet.
    pub fn claude_chat_last_reply_at(&self) -> Option<DateTime<Utc>> {
        self.metadata
            .get("claude_chat_last_reply_at")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
    }

    /// Record that one Claude-Code chat turn just completed: bump the counter
    /// and stamp `now` as the last-reply timestamp. Caller saves the session.
    pub fn record_claude_chat_turn(&mut self, now: DateTime<Utc>) {
        let next = self.claude_chat_turn_count().saturating_add(1);
        let obj = self.metadata_object_mut();
        obj.insert(
            "claude_chat_turn_count".to_string(),
            serde_json::Value::Number(next.into()),
        );
        obj.insert(
            "claude_chat_last_reply_at".to_string(),
            serde_json::Value::String(now.to_rfc3339()),
        );
    }

    /// True when this session is bound to the Claude-Code chat surface.
    pub fn is_claude_chat(&self) -> bool {
        self.metadata
            .get("mode")
            .and_then(|v| v.as_str())
            .map(|s| s == "claude-chat")
            .unwrap_or(false)
    }

    /// Mark this session as claude-chat mode, binding it to a repo, recording
    /// who started it, and optionally pinning the agent the user picked with
    /// `/cc @<agent>` so thread continuations route to the same vault.
    pub fn mark_claude_chat(&mut self, repo: &str, started_by: &str, agent_id: Option<&str>) {
        self.write_claude_chat_binding(("repo", repo), "project", started_by, agent_id);
    }

    /// Mark this session as claude-chat mode bound to a multi-repo project.
    /// Mutually exclusive with [`Self::mark_claude_chat`] — a session is
    /// either single-repo or project-bound, never both.
    pub fn mark_claude_chat_project(
        &mut self,
        project: &str,
        started_by: &str,
        agent_id: Option<&str>,
    ) {
        self.write_claude_chat_binding(("project", project), "repo", started_by, agent_id);
    }

    fn write_claude_chat_binding(
        &mut self,
        (binding_key, binding_value): (&str, &str),
        clear_key: &str,
        started_by: &str,
        agent_id: Option<&str>,
    ) {
        let obj = self.metadata_object_mut();
        obj.insert(
            "mode".to_string(),
            serde_json::Value::String("claude-chat".to_string()),
        );
        obj.insert(
            binding_key.to_string(),
            serde_json::Value::String(binding_value.to_string()),
        );
        obj.remove(clear_key);
        obj.insert(
            "started_by".to_string(),
            serde_json::Value::String(started_by.to_string()),
        );
        match agent_id {
            Some(id) => {
                obj.insert(
                    "chat_agent_id".to_string(),
                    serde_json::Value::String(id.to_string()),
                );
            }
            None => {
                obj.remove("chat_agent_id");
            }
        }
    }

    /// The repo bound to a claude-chat session.
    pub fn claude_chat_repo(&self) -> Option<&str> {
        self.metadata.get("repo").and_then(|v| v.as_str())
    }

    /// The project bound to a claude-chat session, or `None` for single-repo
    /// (legacy) bindings.
    pub fn claude_chat_project(&self) -> Option<&str> {
        self.metadata.get("project").and_then(|v| v.as_str())
    }

    /// The agent id pinned by the `/cc` slash command that opened this chat,
    /// or `None` if the thread predates agent pinning.
    pub fn claude_chat_agent_id(&self) -> Option<&str> {
        self.metadata.get("chat_agent_id").and_then(|v| v.as_str())
    }

    /// Record which coding chat opened this thread (`cc` or `cx`), so its
    /// turns keep running on that engine.
    pub fn set_claude_chat_engine(&mut self, engine: &str) {
        self.metadata_object_mut().insert(
            "chat_engine".to_string(),
            serde_json::Value::String(engine.to_string()),
        );
    }

    /// The coding chat this thread was opened with; `None` for a thread that
    /// predates `/cx`, which is a `/cc` thread.
    pub fn claude_chat_engine(&self) -> Option<&str> {
        self.metadata.get("chat_engine").and_then(|v| v.as_str())
    }

    /// Pin the model and effort the thread's turns run on, as chosen on the
    /// command that opened it. `None` leaves the chat's own default in force.
    pub fn set_claude_chat_choice(&mut self, model: Option<&str>, effort: Option<&str>) {
        let metadata = self.metadata_object_mut();
        for (key, value) in [("chat_model", model), ("chat_effort", effort)] {
            match value {
                Some(v) => {
                    metadata.insert(key.to_string(), serde_json::Value::String(v.to_string()));
                }
                None => {
                    metadata.remove(key);
                }
            }
        }
    }

    pub fn claude_chat_model(&self) -> Option<&str> {
        self.metadata.get("chat_model").and_then(|v| v.as_str())
    }

    pub fn claude_chat_effort(&self) -> Option<&str> {
        self.metadata.get("chat_effort").and_then(|v| v.as_str())
    }

    /// Add a user message.
    pub fn add_user_message(&mut self, content: &str, external_id: Option<&str>) -> String {
        self.add_user_message_with_attachments(content, external_id, None)
    }

    /// Add a user message with optional file/image attachments.
    pub fn add_user_message_with_attachments(
        &mut self,
        content: &str,
        external_id: Option<&str>,
        attachments: Option<Vec<SessionAttachment>>,
    ) -> String {
        self.add_user_message_with_trust(content, external_id, attachments, None)
    }

    /// Add a user message with attachments and an explicit trust classification.
    /// Trust drives prompt-injection wrapping at LLM-assembly time, so ingress
    /// paths (Slack, gateway) call this directly.
    pub fn add_user_message_with_trust(
        &mut self,
        content: &str,
        external_id: Option<&str>,
        attachments: Option<Vec<SessionAttachment>>,
        trust_level: Option<crate::trust::TrustLevel>,
    ) -> String {
        let msg = SessionMessage {
            id: Uuid::new_v4().to_string(),
            role: MessageRole::User,
            content: content.to_string(),
            tool_data: None,
            timestamp: Utc::now(),
            tokens: estimate_tokens(content),
            external_id: external_id.map(String::from),
            attachments,
            trust_level,
            actor: None,
            desk: None,
        };
        let id = msg.id.clone();
        self.total_tokens += msg.tokens as u64;
        self.messages.push(msg);
        self.updated_at = Utc::now();
        id
    }

    /// Add an assistant message.
    pub fn add_assistant_message(&mut self, content: &str, tokens: u32) -> String {
        let msg = SessionMessage {
            id: Uuid::new_v4().to_string(),
            role: MessageRole::Assistant,
            content: content.to_string(),
            tool_data: None,
            timestamp: Utc::now(),
            tokens,
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
            desk: None,
        };
        let id = msg.id.clone();
        self.total_tokens += tokens as u64;
        self.messages.push(msg);
        self.updated_at = Utc::now();
        id
    }

    /// Add a system message.
    pub fn add_system_message(&mut self, content: &str) {
        let msg = SessionMessage {
            id: Uuid::new_v4().to_string(),
            role: MessageRole::System,
            content: content.to_string(),
            tool_data: None,
            timestamp: Utc::now(),
            tokens: estimate_tokens(content),
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
            desk: None,
        };
        self.total_tokens += msg.tokens as u64;
        self.messages.push(msg);
        self.updated_at = Utc::now();
    }

    /// Set or update the system message.
    ///
    /// If a system message already exists, it is replaced in-place with updated content.
    /// If no system message exists, a new one is prepended.
    pub fn set_system_message(&mut self, content: &str) {
        let new_tokens = estimate_tokens(content);
        if let Some(existing) = self
            .messages
            .iter_mut()
            .find(|m| m.role == MessageRole::System)
        {
            // Adjust total token count
            self.total_tokens =
                self.total_tokens.saturating_sub(existing.tokens as u64) + new_tokens as u64;
            existing.content = content.to_string();
            existing.tokens = new_tokens;
            existing.timestamp = Utc::now();
        } else {
            // No system message yet — prepend one
            let msg = SessionMessage {
                id: Uuid::new_v4().to_string(),
                role: MessageRole::System,
                content: content.to_string(),
                tool_data: None,
                timestamp: Utc::now(),
                tokens: new_tokens,
                external_id: None,
                attachments: None,
                trust_level: None,
                actor: None,
                desk: None,
            };
            self.total_tokens += new_tokens as u64;
            self.messages.insert(0, msg);
        }
        self.updated_at = Utc::now();
    }

    /// Get messages formatted for the LLM (respecting context window).
    pub fn context_messages(&self, max_tokens: u32) -> Vec<&SessionMessage> {
        let mut result = Vec::new();
        let mut token_count = 0u32;

        // Always include system messages
        let system_msgs: Vec<&SessionMessage> = self
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::System)
            .collect();
        for msg in &system_msgs {
            token_count += msg.tokens;
        }

        // Then take as many recent messages as fit
        let non_system: Vec<&SessionMessage> = self
            .messages
            .iter()
            .filter(|m| m.role != MessageRole::System)
            .collect();

        let mut recent = Vec::new();
        for msg in non_system.iter().rev() {
            if token_count + msg.tokens > max_tokens {
                break;
            }
            token_count += msg.tokens;
            recent.push(*msg);
        }
        recent.reverse();

        result.extend(system_msgs);
        result.extend(recent);
        result
    }

    /// Add a tool call message with full metadata. Caps `input` at the
    /// storage budget so a single bulk-write call cannot bloat the on-disk
    /// transcript — see [`crate::tool_input::shrink_tool_input`] for the
    /// elision shape.
    pub fn add_tool_message(
        &mut self,
        tool_name: &str,
        input: &serde_json::Value,
        output: &serde_json::Value,
        success: bool,
        duration_ms: u64,
        error: Option<&str>,
    ) -> String {
        let msg = SessionMessage::tool(tool_name, input, output, success, duration_ms, error);
        let id = msg.id.clone();
        self.push_message(msg);
        id
    }

    /// Append a message built elsewhere, such as a tool row the turn journal
    /// already holds under the same id.
    pub fn push_message(&mut self, message: SessionMessage) {
        self.messages.push(message);
        self.updated_at = Utc::now();
    }

    /// Set or clear the per-session model override.
    pub fn set_model_override(&mut self, model: Option<String>) {
        self.model_override = model;
        self.updated_at = Utc::now();
    }

    /// Record a compaction event in the session history. One that replaced
    /// messages also clears the observed-tokens sample: it described the
    /// pre-compaction prompt, and keeping it would re-trigger compaction
    /// against a context that just shrank. A pruning keeps it: a tool row
    /// replays capped or elided whatever its stored size and counts for
    /// nothing in the message estimate, so without the sample a session the
    /// provider measured over the threshold would read as small the moment
    /// any row was pruned, and never be summarised. The
    /// `estimate_calibration` ratio deliberately survives either — it
    /// describes how the session's content tokenises, not any one prompt.
    pub fn record_compaction(&mut self, event: CompactionEvent) {
        let replaced_messages = event.kind != CompactionKind::Pruning;
        self.compaction_history.push(event);
        if let Some(obj) = self.metadata.as_object_mut().filter(|_| replaced_messages) {
            obj.remove("observed_input_tokens");
        }
        self.updated_at = Utc::now();
    }

    /// Record the ACTUAL input-token count the provider reported for the last
    /// run. The estimate-based accounting is blind to ephemeral per-turn
    /// blocks (code-context injection, thread history, lessons) that wrap the
    /// stored message; this sample lets the next turn's compaction check see
    /// the real prompt size.
    pub fn record_observed_input_tokens(&mut self, tokens: u32) {
        self.metadata_object_mut()
            .insert("observed_input_tokens".into(), serde_json::json!(tokens));
    }

    /// The provider-reported input-token count from the last run (0 when no
    /// sample exists). Already includes the reserved floor — comparable
    /// directly against the compaction trigger.
    pub fn observed_input_tokens(&self) -> u32 {
        self.metadata
            .get("observed_input_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v.min(u32::MAX as u64) as u32)
            .unwrap_or(0)
    }

    /// How far real prompts run ahead of the message-token estimate for this
    /// session — an EWMA of observed/estimate clamped to
    /// [`CALIBRATION_MIN`], [`CALIBRATION_MAX`]. `1.0` (the default) leaves the
    /// estimate untouched. Unlike the raw observed sample, this survives
    /// compaction: the ratio describes how this session's content tokenises,
    /// not any one prompt.
    pub fn estimate_calibration(&self) -> f32 {
        self.metadata
            .get("estimate_calibration")
            .and_then(|v| v.as_f64())
            .map(|v| (v as f32).clamp(Self::CALIBRATION_MIN, Self::CALIBRATION_MAX))
            .unwrap_or(Self::CALIBRATION_MIN)
    }

    /// Fold one provider-reported prompt size into the calibration ratio.
    /// Numerator and denominator cover the same quantity — observed minus the
    /// reserved floor vs the message estimate — so the ratio scales only the
    /// message term (the reserved floor is already sized from real schemas).
    pub fn record_estimate_calibration(&mut self, observed: u32, reserved: ReservedContext) {
        let message_estimate = self.non_system_message_tokens().max(1);
        let message_observed = observed.saturating_sub(reserved.total()).max(1);
        let sample = (message_observed as f32 / message_estimate as f32)
            .clamp(Self::CALIBRATION_MIN, Self::CALIBRATION_MAX);
        let previous = self.estimate_calibration();
        let next = previous + Self::CALIBRATION_ALPHA * (sample - previous);
        self.metadata_object_mut()
            .insert("estimate_calibration".into(), serde_json::json!(next));
    }

    const CALIBRATION_ALPHA: f32 = 0.3;
    const CALIBRATION_MIN: f32 = 1.0;
    const CALIBRATION_MAX: f32 = 4.0;

    /// Estimate the total token count across all messages.
    pub fn estimated_total_tokens(&self) -> u32 {
        self.messages.iter().map(|m| m.tokens).sum()
    }

    /// Sum of message tokens excluding the system prompt. The request sends the
    /// system prompt once via the dedicated system parameter (conversation
    /// history is assembled without System-role messages), and
    /// `ReservedContext::system_prompt_tokens` accounts for it — so the
    /// compaction threshold must exclude it here to avoid counting it twice.
    pub fn non_system_message_tokens(&self) -> u32 {
        self.messages
            .iter()
            .filter(|m| m.role != MessageRole::System)
            .map(|m| m.tokens)
            .sum()
    }

    /// Number of conversational (User/Assistant) messages — the volume signal
    /// for voice rollover, where tool and system entries don't represent
    /// spoken exchange.
    pub fn conversational_message_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::User | MessageRole::Assistant))
            .count()
    }

    /// Estimate total context usage including fixed per-request overhead
    /// (system prompt, tool schemas, response reservation).
    pub fn estimated_total_with_reserved(&self, reserved: ReservedContext) -> u32 {
        self.non_system_message_tokens()
            .saturating_add(reserved.total())
    }

    /// The context size the compaction thresholds compare against: the
    /// calibrated estimate, or the last provider-reported input-token sample
    /// when that is larger. The raw estimate alone is blind to ephemeral
    /// per-turn blocks (code-context injection, thread history), which let
    /// real prompts run tens of thousands of tokens past what the accounting
    /// saw; the calibration ratio carries that drift across compactions,
    /// which clear the raw sample. Only the message term is scaled — the
    /// reserved floor is already sized from real schemas.
    fn compaction_pressure_tokens(&self, reserved: ReservedContext) -> u32 {
        let calibrated =
            (self.non_system_message_tokens() as f64 * self.estimate_calibration() as f64) as u32;
        calibrated
            .saturating_add(reserved.total())
            .max(self.observed_input_tokens())
    }

    /// Check whether compaction should be triggered based on the config.
    pub fn needs_compaction(&self, config: &CompactionConfig, reserved: ReservedContext) -> bool {
        self.compaction_pressure_tokens(reserved) > config.trigger_threshold()
    }

    /// Check whether compaction is urgently needed (near the hard context limit).
    /// When true, compaction must run synchronously before the LLM call.
    pub fn needs_urgent_compaction(
        &self,
        config: &CompactionConfig,
        reserved: ReservedContext,
    ) -> bool {
        self.compaction_pressure_tokens(reserved) > config.urgent_threshold()
    }

    // ── Reversible tool-output retrieval (CCR, Phase 1) ─────────────────────
    // Before a compaction op replaces a
    // tool message's `output` with a truncation/eviction marker, the pristine
    // original is copied into `output_full` and keyed by a `retrieve_handle`
    // embedded in the marker, so the model can recover it with the `tool.retrieve`
    // tool instead of re-running the tool. `output_full` is NEVER read by any
    // model-facing path — only `tool.retrieve` reads it (and tokenises on egress).

    /// Total bytes currently retained in `output_full` stashes across the
    /// session — the growth this feature adds. Used to enforce a per-session cap.
    const CCR_STASH_CAP_BYTES: usize = 2 * 1024 * 1024;

    fn stashed_bytes_in(messages: &[SessionMessage]) -> usize {
        messages
            .iter()
            .filter_map(|m| m.tool_data.as_ref())
            .filter_map(|td| td.get("output_full"))
            .map(|v| match v {
                serde_json::Value::String(s) => s.len(),
                other => other.to_string().len(),
            })
            .sum()
    }

    /// Preserve a tool message's pristine `output` under `output_full` before a
    /// destructive compaction op overwrites it, returning the retrieval handle to
    /// embed in the marker. `None` when the output is below `min_chars` (not worth
    /// stashing) or the session is over [`CCR_STASH_CAP_BYTES`] (lossy fallback).
    ///
    /// Idempotent and keyed on the **presence of `retrieve_handle`**, not on
    /// parsing the marker: the first destructive op captures the pristine output;
    /// later passes return the existing handle without re-copying, so repeated
    /// compaction never overwrites the original with an already-truncated value.
    fn stash_output_full(
        msg: &mut SessionMessage,
        min_chars: usize,
        budget_remaining: &mut usize,
    ) -> Option<String> {
        let tool_data = msg.tool_data.as_mut()?.as_object_mut()?;
        if let Some(h) = tool_data.get("retrieve_handle").and_then(|v| v.as_str()) {
            return Some(h.to_string()); // already stashed — pristine capture stands
        }
        let output = tool_data.get("output")?;
        let output_str = match output {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if output_str.len() < min_chars || output_str.len() > *budget_remaining {
            return None;
        }
        let tool_name = tool_data.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let mut hasher = blake3::Hasher::new();
        hasher.update(tool_name.as_bytes());
        hasher.update(&(output_str.len() as u64).to_le_bytes());
        hasher.update(output_str.as_bytes());
        let handle = format!("h:{}", &hasher.finalize().to_hex()[..10]);

        let original = output.clone();
        tool_data.insert("output_full".to_string(), original);
        tool_data.insert(
            "retrieve_handle".to_string(),
            serde_json::Value::String(handle.clone()),
        );
        *budget_remaining = budget_remaining.saturating_sub(output_str.len());
        Some(handle)
    }

    /// Suffix appended to a marker so the model knows how to recover the original.
    fn retrieve_hint(handle: &Option<String>) -> String {
        match handle {
            Some(h) => format!(" — recover the full output with tool.retrieve(handle=\"{h}\")"),
            None => String::new(),
        }
    }

    /// Prune large tool input/output payloads in-place, replacing content
    /// that exceeds the configured caps with truncation markers.
    ///
    /// Lightweight compaction strategy that reduces token usage without an
    /// LLM summarisation call. Only modifies messages older than `keep_recent`
    /// (counted from the end). A single message that has both bloated input
    /// and bloated output counts once in the returned pruned-message tally.
    ///
    /// The pristine output is stashed under `output_full` before truncation so
    /// the model can recover it via `tool.retrieve` (Phase 1, CCR).
    pub fn prune_tool_outputs(
        &mut self,
        output_max_chars: usize,
        input_max_chars: usize,
        keep_recent: usize,
    ) -> usize {
        let total = self.messages.len();
        let prune_boundary = total.saturating_sub(keep_recent);
        let mut pruned = 0;
        let input_field_max = (input_max_chars / 4).max(1);
        let mut stash_budget =
            Self::CCR_STASH_CAP_BYTES.saturating_sub(Self::stashed_bytes_in(&self.messages));

        for msg in self.messages[..prune_boundary].iter_mut() {
            if msg.role != MessageRole::Tool {
                continue;
            }
            let mut msg_pruned = false;

            // Preserve the pristine output for tool.retrieve before truncating it.
            let handle = Self::stash_output_full(msg, output_max_chars, &mut stash_budget);

            if let Some(ref mut tool_data) = msg.tool_data {
                if let Some(output) = tool_data.get("output") {
                    let output_str = output.to_string();
                    if output_str.len() > output_max_chars {
                        let truncated = format!(
                            "{}... [output truncated, was {} chars]{}",
                            crate::text::truncate_str(&output_str, output_max_chars),
                            output_str.len(),
                            Self::retrieve_hint(&handle),
                        );
                        if let Some(obj) = tool_data.as_object_mut() {
                            obj.insert("output".to_string(), serde_json::Value::String(truncated));
                        }
                        msg_pruned = true;
                    }
                }

                if let Some(obj) = tool_data.as_object_mut() {
                    if let Some(input_value) = obj.get_mut("input") {
                        let outcome = crate::tool_input::shrink_tool_input(
                            input_value,
                            input_field_max,
                            input_max_chars,
                        );
                        if outcome.shrunk {
                            msg_pruned = true;
                        }
                    }
                }
            }

            if msg.content.len() > output_max_chars {
                let old_tokens = msg.tokens;
                let original_len = msg.content.len();
                msg.content = format!(
                    "{}... [truncated, was {} chars]",
                    crate::text::truncate_str(&msg.content, output_max_chars),
                    original_len
                );
                msg.tokens = old_tokens.min(estimate_tokens(&msg.content));
                self.total_tokens =
                    self.total_tokens.saturating_sub(old_tokens as u64) + msg.tokens as u64;
                msg_pruned = true;
            }

            if msg_pruned {
                pruned += 1;
            }
        }

        if pruned > 0 {
            self.updated_at = Utc::now();
        }
        pruned
    }

    /// Selectively evict tool outputs from the context window.
    ///
    /// Replaces matching tool output content with lightweight markers containing
    /// enough metadata for the agent to re-fetch if needed. This is a precise,
    /// surgical alternative to full compaction.
    ///
    /// # Parameters
    /// - `tool_filter`: If Some, only evict outputs from these tool names. If None, all tools.
    /// - `older_than_round`: If Some, only evict outputs at message index < this value.
    /// - `keep_last`: Always preserve the N most recent outputs per tool name.
    /// - `min_chars`: Skip evicting outputs smaller than this threshold.
    pub fn evict_tool_outputs(
        &mut self,
        tool_filter: Option<&[String]>,
        older_than_round: Option<usize>,
        keep_last: usize,
        min_chars: usize,
    ) -> EvictionResult {
        use std::collections::HashMap;

        // Phase 1: Collect indices of tool messages grouped by tool name
        let mut tool_groups: HashMap<String, Vec<usize>> = HashMap::new();

        for (idx, msg) in self.messages.iter().enumerate() {
            if msg.role != MessageRole::Tool {
                continue;
            }
            let tool_name = msg
                .tool_data
                .as_ref()
                .and_then(|td| td.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if tool_name.is_empty() {
                continue;
            }

            // Apply tool name filter
            if let Some(filter) = tool_filter {
                if !filter.iter().any(|f| f == &tool_name) {
                    continue;
                }
            }

            tool_groups.entry(tool_name).or_default().push(idx);
        }

        // Phase 2: Determine which indices to evict (respecting keep_last)
        let mut evict_indices: Vec<usize> = Vec::new();

        for indices in tool_groups.values() {
            // Indices are already in chronological order (ascending)
            let protect_from = indices.len().saturating_sub(keep_last);
            for (pos, &msg_idx) in indices.iter().enumerate() {
                if pos >= protect_from {
                    // Protected by keep_last
                    continue;
                }
                // Apply round filter
                if let Some(round_boundary) = older_than_round {
                    if msg_idx >= round_boundary {
                        continue;
                    }
                }
                evict_indices.push(msg_idx);
            }
        }

        // Phase 3: Perform eviction
        let mut evicted = 0usize;
        let mut tokens_freed = 0u64;
        let mut stash_budget =
            Self::CCR_STASH_CAP_BYTES.saturating_sub(Self::stashed_bytes_in(&self.messages));

        for &idx in &evict_indices {
            let msg = &self.messages[idx];
            let output_str = msg
                .tool_data
                .as_ref()
                .and_then(|td| td.get("output"))
                .map(|v| v.to_string())
                .unwrap_or_default();

            if output_str.len() < min_chars {
                continue;
            }

            let tool_name = self.messages[idx]
                .tool_data
                .as_ref()
                .and_then(|td| td.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let path_hint = self.messages[idx]
                .tool_data
                .as_ref()
                .and_then(|td| td.get("input"))
                .and_then(|input| {
                    input
                        .get("path")
                        .or_else(|| input.get("pattern"))
                        .or_else(|| input.get("repo"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("");

            let marker = if path_hint.is_empty() {
                format!(
                    "[Output evicted — was {} chars from {}]",
                    output_str.len(),
                    tool_name
                )
            } else {
                format!(
                    "[Output evicted — was {} chars from {} of {}]",
                    output_str.len(),
                    tool_name,
                    path_hint
                )
            };

            let msg = &mut self.messages[idx];
            let old_tokens = msg.tokens;

            // Preserve the pristine output for tool.retrieve, then point the
            // marker at the recovery handle (lossless eviction).
            let handle = Self::stash_output_full(msg, min_chars, &mut stash_budget);
            let marker = format!("{marker}{}", Self::retrieve_hint(&handle));

            // Replace tool_data output
            if let Some(ref mut tool_data) = msg.tool_data {
                if let Some(obj) = tool_data.as_object_mut() {
                    obj.insert(
                        "output".to_string(),
                        serde_json::Value::String(marker.clone()),
                    );
                }
            }

            // Replace message content
            msg.content = marker.clone();
            msg.tokens = estimate_tokens(&msg.content);

            let freed = old_tokens.saturating_sub(msg.tokens);
            tokens_freed += freed as u64;
            self.total_tokens = self.total_tokens.saturating_sub(freed as u64);

            evicted += 1;
        }

        // Count kept: total tool messages matching filters minus evicted
        let total_matching: usize = tool_groups.values().map(|v| v.len()).sum();
        let kept = total_matching.saturating_sub(evicted);

        if evicted > 0 {
            self.updated_at = Utc::now();
        }

        EvictionResult {
            evicted,
            kept,
            tokens_freed,
        }
    }

    /// Evict all tool outputs from messages before a given round (message index).
    ///
    /// Unlike `evict_tool_outputs`, this has no keep_last protection and no
    /// tool name filter — it evicts everything before the specified round.
    pub fn evict_by_round(&mut self, before_round: usize, min_chars: usize) -> EvictionResult {
        let mut evicted = 0usize;
        let mut kept = 0usize;
        let mut tokens_freed = 0u64;
        let mut stash_budget =
            Self::CCR_STASH_CAP_BYTES.saturating_sub(Self::stashed_bytes_in(&self.messages));

        let boundary = before_round.min(self.messages.len());

        for idx in 0..boundary {
            let msg = &self.messages[idx];
            if msg.role != MessageRole::Tool {
                continue;
            }

            let tool_name = msg
                .tool_data
                .as_ref()
                .and_then(|td| td.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if tool_name.is_empty() {
                kept += 1;
                continue;
            }

            let output_str = msg
                .tool_data
                .as_ref()
                .and_then(|td| td.get("output"))
                .map(|v| v.to_string())
                .unwrap_or_default();

            if output_str.len() < min_chars {
                kept += 1;
                continue;
            }

            let path_hint = msg
                .tool_data
                .as_ref()
                .and_then(|td| td.get("input"))
                .and_then(|input| {
                    input
                        .get("path")
                        .or_else(|| input.get("pattern"))
                        .or_else(|| input.get("repo"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("");

            let marker = if path_hint.is_empty() {
                format!(
                    "[Output evicted — was {} chars from {}]",
                    output_str.len(),
                    tool_name
                )
            } else {
                format!(
                    "[Output evicted — was {} chars from {} of {}]",
                    output_str.len(),
                    tool_name,
                    path_hint
                )
            };

            let msg = &mut self.messages[idx];
            let old_tokens = msg.tokens;

            // Preserve the pristine output for tool.retrieve (lossless eviction).
            let handle = Self::stash_output_full(msg, min_chars, &mut stash_budget);
            let marker = format!("{marker}{}", Self::retrieve_hint(&handle));

            if let Some(ref mut tool_data) = msg.tool_data {
                if let Some(obj) = tool_data.as_object_mut() {
                    obj.insert(
                        "output".to_string(),
                        serde_json::Value::String(marker.clone()),
                    );
                }
            }

            msg.content = marker;
            msg.tokens = estimate_tokens(&msg.content);

            let freed = old_tokens.saturating_sub(msg.tokens);
            tokens_freed += freed as u64;
            self.total_tokens = self.total_tokens.saturating_sub(freed as u64);

            evicted += 1;
        }

        // Also count tool messages after the boundary as kept
        for msg in &self.messages[boundary..] {
            if msg.role == MessageRole::Tool
                && msg
                    .tool_data
                    .as_ref()
                    .and_then(|td| td.get("name"))
                    .and_then(|v| v.as_str())
                    .is_some()
            {
                kept += 1;
            }
        }

        if evicted > 0 {
            self.updated_at = Utc::now();
        }

        EvictionResult {
            evicted,
            kept,
            tokens_freed,
        }
    }

    /// Perform a full compaction: replace old messages with a summary message.
    ///
    /// Keeps the system prompt (first system message) and the most recent
    /// `keep_recent` messages. Everything in between is replaced with a single
    /// system-role compaction summary.
    ///
    /// Returns the `CompactionEvent` metadata.
    pub fn compact_with_summary(
        &mut self,
        summary: &str,
        keep_recent: usize,
        kind: CompactionKind,
    ) -> CompactionEvent {
        let tokens_before = self.estimated_total_tokens() as u64;
        let total = self.messages.len();

        let keep_from = total.saturating_sub(keep_recent);

        let mut new_messages: Vec<SessionMessage> = Vec::new();

        // 1. Keep non-summary system messages before the keep boundary.
        //    Previous compaction summaries are dropped — they get folded into
        //    the new summary to prevent unbounded accumulation.
        let mut kept_system = 0usize;
        for msg in &self.messages[..keep_from] {
            if msg.role == MessageRole::System && !msg.is_compaction_summary() {
                new_messages.push(msg.clone());
                kept_system += 1;
            }
        }

        // 2. Insert the new compaction summary
        let summary_msg = SessionMessage {
            id: Uuid::new_v4().to_string(),
            role: MessageRole::System,
            content: format!(
                "{COMPACTION_SUMMARY_PREFIX} — earlier conversation condensed]\n\n{summary}",
            ),
            tool_data: None,
            timestamp: Utc::now(),
            tokens: estimate_tokens(summary),
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
            desk: None,
        };
        new_messages.push(summary_msg);

        // 3. Keep recent messages (including any system messages in that range)
        new_messages.extend_from_slice(&self.messages[keep_from..]);

        let messages_compacted = keep_from.saturating_sub(kept_system);

        self.messages = new_messages;

        // Recalculate total tokens
        let tokens_after = self.estimated_total_tokens() as u64;
        self.total_tokens = tokens_after;
        self.updated_at = Utc::now();

        let event = CompactionEvent {
            timestamp: Utc::now(),
            messages_compacted,
            tokens_before,
            tokens_after,
            kind,
        };
        // Through `record_compaction` (not a direct push) so the observed-
        // tokens sample clears on EVERY compaction path — a stale sample
        // describing the pre-compaction prompt would re-trigger compaction
        // against a context that just shrank.
        self.record_compaction(event.clone());
        event
    }

    /// Get the total message count. When this is an unhydrated shell
    /// (`messages` empty but the on-disk session has bodies), falls back
    /// to the cached count stashed at load time. Hydrated sessions
    /// always answer from `messages.len()`.
    pub fn message_count(&self) -> usize {
        if !self.messages.is_empty() {
            self.messages.len()
        } else {
            self.cached_message_count.unwrap_or(0)
        }
    }

    /// Get the last N messages.
    pub fn recent_messages(&self, n: usize) -> &[SessionMessage] {
        let start = self.messages.len().saturating_sub(n);
        &self.messages[start..]
    }
}

/// Rough token estimate (~4 chars per token).
pub fn estimate_tokens(text: &str) -> u32 {
    (text.len() as u32 / 4).max(1)
}

/// Session store trait for persistence backends.
/// Context window usage for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionContextUsage {
    /// Estimated token count for all current messages.
    pub tokens_used: u32,
    /// Maximum context tokens (defaults to 200,000).
    pub tokens_max: u32,
    /// Fraction of the context window used (0.0–1.0).
    pub utilization: f32,
    /// Whether compaction should be triggered (based on default 80% threshold).
    pub needs_compaction: bool,
    /// Per-session model override (if set).
    pub model_override: Option<String>,
    /// Number of messages in the session.
    pub message_count: usize,
}

/// A tool call record extracted from a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// Message ID of the tool call message.
    pub message_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Tool input parameters.
    pub input: serde_json::Value,
    /// Tool output.
    pub output: serde_json::Value,
    /// Whether the call succeeded.
    pub success: bool,
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// When the tool was called.
    pub timestamp: DateTime<Utc>,
}

/// A search result from session transcript search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSearchResult {
    /// Session ID containing the match.
    pub session_id: String,
    /// Session label (if set).
    pub label: Option<String>,
    /// Message ID of the matching message.
    pub message_id: String,
    /// Role of the message.
    pub role: MessageRole,
    /// An excerpt of the content with context around the match.
    pub excerpt: String,
    /// When the message was recorded.
    pub timestamp: DateTime<Utc>,
}

/// Outcome of redacting one session's stored secrets at rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedactionStats {
    /// Live messages examined.
    pub messages_scanned: usize,
    /// Live messages whose content or `tool_data` was rewritten.
    pub messages_redacted: usize,
    /// Append-only archive lines rewritten.
    pub archive_messages_redacted: usize,
}

impl RedactionStats {
    /// Whether anything was (or would be) redacted.
    pub fn changed(&self) -> bool {
        self.messages_redacted > 0 || self.archive_messages_redacted > 0
    }
}

/// Redact secrets in one message's text and `tool_data` string leaves.
/// Returns whether anything changed. Reuses the ingress pattern
/// catalogue, so `[[secret:…]]` markers and already-masked values are
/// left untouched (idempotent).
pub(crate) fn redact_message_secrets(msg: &mut SessionMessage) -> bool {
    let new_content = crate::redact::redact_secrets(&msg.content);
    let content_changed = new_content != msg.content;
    if content_changed {
        msg.content = new_content;
    }
    let tool_changed = msg
        .tool_data
        .as_mut()
        .is_some_and(|td| crate::redact::redact_json_strings(td, 32));
    content_changed || tool_changed
}

pub(crate) fn redact_messages_in_place(messages: &mut [SessionMessage]) -> RedactionStats {
    messages
        .iter_mut()
        .fold(RedactionStats::default(), |mut acc, msg| {
            acc.messages_scanned += 1;
            if redact_message_secrets(msg) {
                acc.messages_redacted += 1;
            }
            acc
        })
}

/// A session [`SessionStore::reassign_owner`] moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reassigned {
    pub session_id: String,
    /// The channel it is on now.
    pub channel: String,
    /// Whether it was moved off its channel because the new owner already had
    /// an open session there.
    pub renamed: bool,
}

/// The channel an open session moves to when its new owner already has an
/// open session on its own: both conversations are kept, and the moved one
/// says where it came from.
pub fn reassigned_channel(channel: &str, tag: &str) -> String {
    format!("{channel}#from-{tag}")
}

/// A turn begun by an instance that never finished it: the session, when the
/// turn started, and the id of the user message it was answering.
///
/// `started_at` and `message_id` are what let a sweep decide whether the turn
/// already produced an answer; a record that carries neither can never be
/// shown to have been answered, so it is cleared rather than resumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedTurn {
    pub session_id: String,
    pub started_at: Option<DateTime<Utc>>,
    pub message_id: Option<String>,
    /// Fencing token of the record, to hand back to [`SessionStore::end_turn`]
    /// so a sweep can only clear the record it actually judged.
    pub token: String,
    /// Who the turn ran as and billed to; empty on records written before
    /// it was kept.
    pub principal: TurnPrincipal,
}

/// Who a turn runs as and whom its spend is charged to, kept where the
/// ambient identity does not reach: a turn resumed after a restart, or an
/// isolated run detached from the turn that started it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnPrincipal {
    #[serde(default)]
    pub run_as: Option<String>,
    /// An explicit billing identity; absent, spend is charged to `run_as`.
    #[serde(default)]
    pub bill_to: Option<String>,
}

impl TurnPrincipal {
    pub fn run_as(run_as: Option<String>) -> Self {
        Self {
            run_as,
            bill_to: None,
        }
    }

    /// The identity the turn's spend is charged to ([`Payer::charged`]).
    pub fn billing(&self) -> Option<&str> {
        Payer::charged(self.run_as.as_deref(), self.bill_to.as_deref())
    }

    /// The people this work was done for: whoever it ran as or is billed
    /// to, a `system:` or `agent:` lane being nobody.
    fn people(&self) -> impl Iterator<Item = &str> {
        [&self.run_as, &self.bill_to]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .filter(|id| !id.starts_with("system:") && !id.starts_with("agent:"))
    }

    /// The person a turn reads as: whoever it runs as, or, for a lane or a
    /// turn naming nobody, the person it is billed to. A turn that runs as a
    /// person never reads as the other person its spend is billed to.
    fn reads_as(&self) -> Option<&str> {
        self.people().next()
    }

    /// Whether `reader` may see what this work read and produced: one of the
    /// people it was done for. Work done for nobody is a system run, which
    /// every reader sees.
    pub fn readable_by(&self, reader: &TurnPrincipal) -> bool {
        self.people().next().is_none()
            || reader
                .reads_as()
                .is_some_and(|me| self.people().any(|person| person == me))
    }
}

/// Filename suffix of a JSONL store's turn-record sidecar.
pub const TURN_RECORD_SUFFIX: &str = ".turn.json";
/// Filename suffix of a JSONL store's turn-journal sidecar.
pub const TURN_JOURNAL_SUFFIX: &str = ".journal.jsonl";

/// Whether a file in a sessions directory is per-install state rather than
/// part of a conversation: a turn record or a turn journal names this
/// install's instances and workers, so anything copying the directory (agent
/// export and import) leaves it behind.
pub fn is_install_state(file_name: &str) -> bool {
    file_name.ends_with(TURN_RECORD_SUFFIX) || file_name.ends_with(TURN_JOURNAL_SUFFIX)
}

/// One step of a turn in flight, kept beside its session rather than in it,
/// so a turn that dies can be taken up where it stopped. Entries are keyed by
/// the tool call they belong to and [`JournalEntry::key`]; writing one twice
/// is a no-op.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    /// The call id the runner gave the tool call.
    pub call: String,
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub step: JournalStep,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalStep {
    /// The call started work that outlives this process; `plan` is what the
    /// tool needs to collect that work later.
    Pending {
        tool: String,
        /// The call's input as the transcript keeps it, secrets as markers.
        input: serde_json::Value,
        plan: serde_json::Value,
        /// Who a shared session's row for the call belongs to.
        #[serde(default)]
        actor: Option<String>,
    },
    /// The call's work reached `mark`, such as a job it is about to start:
    /// collecting resumes from the latest mark rather than deciding again.
    Checkpoint { mark: String },
    /// The call returned; `message` is its row as the session holds it.
    Finished { message: SessionMessage },
}

impl JournalEntry {
    /// What makes the entry distinct among its call's: a call has one
    /// pending and one finished entry, and a checkpoint per mark.
    pub fn key(&self) -> String {
        match &self.step {
            JournalStep::Pending { .. } => "pending".to_string(),
            JournalStep::Checkpoint { mark } => format!("checkpoint:{mark}"),
            JournalStep::Finished { .. } => "finished".to_string(),
        }
    }
}

/// `entries` in order with later copies of an entry dropped.
fn first_of_each(entries: impl IntoIterator<Item = JournalEntry>) -> Vec<JournalEntry> {
    let mut seen = std::collections::HashSet::new();
    entries
        .into_iter()
        .filter(|entry| seen.insert((entry.call.clone(), entry.key())))
        .collect()
}

/// Apply a metadata patch in place, promoting a non-object value (a fresh
/// session's metadata is `Null`) to an object first.
pub(crate) fn apply_metadata_patch(
    metadata: &mut serde_json::Value,
    patch: serde_json::Map<String, serde_json::Value>,
    remove: &[&str],
) {
    if !metadata.is_object() {
        *metadata = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Some(obj) = metadata.as_object_mut() {
        for key in remove {
            obj.remove(*key);
        }
        obj.extend(patch);
    }
}

/// How many of `sessions` each owner holds.
fn count_owners<'a>(sessions: impl IntoIterator<Item = &'a Session>) -> BTreeMap<String, usize> {
    sessions
        .into_iter()
        .fold(BTreeMap::new(), |mut owners, session| {
            *owners.entry(session.owner.clone()).or_default() += 1;
            owners
        })
}

pub trait SessionStore: Send + Sync {
    /// Snapshot the in-memory cache for memory observability. The default
    /// implementation returns all-zeros — implementations without an
    /// in-process cache (e.g. Postgres-backed stores) leave it as-is and
    /// only those that maintain a HashMap (`JsonlSessionStore`) override.
    fn cache_stats(&self) -> SessionCacheStats {
        SessionCacheStats::default()
    }

    /// Find the active session for a channel/owner combination — the newest
    /// unsealed member — without creating one. Returns `None` when none exists.
    fn find(&self, channel: &str, owner: &str) -> eyre::Result<Option<Session>>;

    /// Get or create a session for a channel/owner combination.
    fn get_or_create(&self, channel: &str, owner: &str, agent_id: &str) -> eyre::Result<Session>;

    /// Save a session.
    fn save(&self, session: &Session) -> eyre::Result<()>;

    /// Save a session and refresh its optimistic-concurrency baseline so the
    /// caller can keep mutating and saving the same in-memory `Session`
    /// without every later save counting as contended. The default is plain
    /// `save` — only stores with revision CAS (Postgres) override it.
    fn save_refreshing(&self, session: &mut Session) -> eyre::Result<()> {
        self.save(session)
    }

    /// Claim the fleet-wide right to run a turn for `session_id`, valid for
    /// `ttl_secs`. Re-entrant: the claiming process wins again (renewal passes
    /// the same `claim_id`; a newer local turn passes a fresh one), and a
    /// claim whose TTL has lapsed is stolen. The default grants
    /// unconditionally — single-process stores serialise turns locally; only
    /// stores shared between hub instances (Postgres) override it.
    fn try_claim_turn(
        &self,
        _session_id: &str,
        _claim_id: &str,
        _ttl_secs: u64,
    ) -> eyre::Result<bool> {
        Ok(true)
    }

    /// Extend this process's own claim `claim_id` by `ttl_secs`. Never
    /// creates a row: `false` means the claim was released or superseded, so
    /// a renewal racing a release cannot resurrect it.
    fn renew_turn_claim(
        &self,
        _session_id: &str,
        _claim_id: &str,
        _ttl_secs: u64,
    ) -> eyre::Result<bool> {
        Ok(true)
    }

    /// Release a turn claim taken with [`Self::try_claim_turn`]. A stale
    /// `claim_id` — superseded by a newer claim — is a no-op, so a finishing
    /// turn can never drop the claim out from under its successor.
    fn release_turn_claim(&self, _session_id: &str, _claim_id: &str) -> eyre::Result<()> {
        Ok(())
    }

    /// Who holds the live turn claim for `session_id`, as
    /// `(owner_instance, seconds_until_expiry)`. `None` when unclaimed or
    /// expired. Read on claim refusal so callers can report an honest wait.
    fn turn_claim_holder(&self, _session_id: &str) -> eyre::Result<Option<(String, u64)>> {
        Ok(None)
    }

    /// Delete every turn claim owned by this process. The shutdown backstop:
    /// after in-flight turns are cancelled, one call guarantees no claim
    /// outlives the process regardless of how individual guards died.
    fn release_owned_turn_claims(&self) -> eyre::Result<u64> {
        Ok(0)
    }

    /// Record this process as alive in the shared liveness roster, and prune
    /// rows stale for over a day. Claim acquisition steals from owners whose
    /// heartbeat has gone stale, freeing a crashed instance's sessions in
    /// seconds instead of a full claim TTL.
    fn heartbeat_instance(&self) -> eyre::Result<()> {
        Ok(())
    }

    /// Atomically seal `sealed` and insert its successor in one step.
    ///
    /// The thread-rollover path seals the active session and mints its
    /// successor; for a store that enforces "one active session per
    /// conversation" with a partial-unique index (Postgres), doing this as
    /// two independent `save`s transiently exposes either zero or two
    /// unsealed rows — racing a concurrent `get_or_create` or tripping the
    /// index. The default does the two `save`s (correct for the cache-backed
    /// file/in-memory stores, which serialise under their own lock); the
    /// Postgres backend overrides it with a single transaction.
    fn seal_and_insert_successor(&self, sealed: &Session, successor: &Session) -> eyre::Result<()> {
        self.save(sealed)?;
        self.save(successor)
    }

    /// Restart the conversation for `channel`/`owner`: seal its active session
    /// and make a bare successor the active one, or create the first session
    /// when there is none. Returns the active session — a concurrent restart
    /// that won the seal+insert race yields its successor instead of failing.
    fn start_fresh(&self, channel: &str, owner: &str, agent_id: &str) -> eyre::Result<Session> {
        let Some(mut current) = self.find(channel, owner)? else {
            return self.get_or_create(channel, owner, agent_id);
        };
        let successor = current.successor();
        current.seal();
        match self.seal_and_insert_successor(&current, &successor) {
            Ok(()) => Ok(successor),
            Err(e) => self
                .find(channel, owner)
                .ok()
                .flatten()
                .filter(|active| active.id != current.id)
                .ok_or(e),
        }
    }

    /// Record that this instance has begun a turn for `session_id`, answering
    /// user message `message_id`. Returns the record's fencing token, which
    /// [`SessionStore::end_turn`] requires — so a turn can only ever end its
    /// own record, never the one a successor opened while it was dying.
    ///
    /// The record lives outside the session value on purpose. A whole-session
    /// `save` is the write designed to lose conflicts — Postgres keeps the
    /// stored metadata on a CAS miss, the file store replaces the metadata
    /// wholesale — so lifecycle state carried by it is silently droppable in
    /// one store and silently erasable in the other. Turn state therefore has
    /// exactly one writer and its own atomic write, like `sealed` and `label`
    /// before it.
    fn begin_turn(
        &self,
        session_id: &str,
        message_id: Option<&str>,
        principal: &TurnPrincipal,
    ) -> eyre::Result<String>;

    /// End the turn record for `session_id` if `token` still identifies it,
    /// returning whether it did. Idempotent, and a no-op against a superseded
    /// record — the same discipline `release_turn_claim` applies to a stale
    /// `claim_id`.
    fn end_turn(&self, session_id: &str, token: &str) -> eyre::Result<bool>;

    /// Append `entry` to the session's turn journal; an entry already there is
    /// left as it is. Like the turn record, the journal lives outside the
    /// session value, so appending never touches the transcript or its
    /// concurrency baseline and is safe while the turn still runs.
    fn journal_append(&self, session_id: &str, entry: &JournalEntry) -> eyre::Result<()>;

    /// The session's turn journal, oldest entry first.
    fn journal(&self, session_id: &str) -> eyre::Result<Vec<JournalEntry>>;

    /// Empty the session's turn journal, once what it recorded is saved in
    /// the session.
    fn journal_clear(&self, session_id: &str) -> eyre::Result<()>;

    /// The keys of every journal holding entries whose key starts with
    /// `prefix`. A journal is keyed by a string, so work that is not a
    /// session's turn, such as a fan-out wave, keeps one too.
    fn journals(&self, prefix: &str) -> eyre::Result<Vec<String>>;

    /// Merge `patch` into a session's metadata and drop `remove`, atomically,
    /// leaving the transcript and the concurrency baseline untouched.
    ///
    /// ONLY for keys a turn owns and writes at the END of its turn, after its
    /// last whole-session save. `save` writes the metadata blob whole, so a
    /// patch applied while a turn is still running is simply overwritten by
    /// that turn's own save — which is why this is not a general-purpose
    /// metadata API. Used at the right moment there is nothing left to
    /// overwrite it, and the value is re-derived next turn anyway, so a lost
    /// race self-heals rather than accumulating.
    ///
    /// `revision` is deliberately not bumped: bumping would make a concurrent
    /// turn's save CAS-miss and lose ITS metadata, which is the failure this
    /// exists to stop.
    fn merge_metadata(
        &self,
        session_id: &str,
        patch: serde_json::Map<String, serde_json::Value>,
        remove: &[&str],
    ) -> eyre::Result<()>;

    /// Turns left unfinished by an instance other than this one — a crashed
    /// process, or one drained mid-turn by a rolling deploy. Our own records
    /// are never returned: they belong to turns running right now, or to stop
    /// paths that clear them.
    fn interrupted_turns(&self) -> eyre::Result<Vec<InterruptedTurn>>;

    /// Get a session by ID.
    fn get(&self, session_id: &str) -> eyre::Result<Option<Session>>;

    /// List sessions, optionally filtered.
    fn list(&self, channel: Option<&str>, limit: usize) -> eyre::Result<Vec<Session>>;

    /// List sessions for one agent that have been updated at or after
    /// `cutoff`. Implementations are encouraged to push the predicate into
    /// the storage layer; the default scans the unfiltered `list` and
    /// filters in Rust, which is correct but wasteful when the store has
    /// many non-matching sessions. The cap caps the worst-case result size
    /// even when the store-level filter under-bounds (e.g. a session count
    /// way past the active window).
    fn list_since(
        &self,
        agent_id: &str,
        cutoff: DateTime<Utc>,
        limit: usize,
    ) -> eyre::Result<Vec<Session>> {
        Ok(self
            .list(None, limit)?
            .into_iter()
            .filter(|s| s.agent_id == agent_id && s.updated_at >= cutoff)
            .collect())
    }

    /// List the sessions belonging to one `owner`, newest-updated first — the
    /// owner-scoped read path for customer-facing conversation history. As
    /// with [`SessionStore::list_since`], implementations are encouraged to
    /// push the predicate into the storage layer; the default scans `list`
    /// and filters in Rust. Returned sessions may be shells (no messages) —
    /// callers needing transcripts fetch the individual session.
    fn list_for_owner(&self, owner: &str, limit: usize) -> eyre::Result<Vec<Session>> {
        // Bounded scan (not usize::MAX): stores convert the limit to i64 for
        // SQL, where a saturated value goes negative and breaks the query.
        let mut sessions: Vec<Session> = self
            .list(None, 10_000)?
            .into_iter()
            .filter(|s| s.owner == owner)
            .collect();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        sessions.truncate(limit);
        Ok(sessions)
    }

    /// How many sessions each owner holds, sealed ones included: what an
    /// identity clean-up reads to find owners that no longer exist.
    fn owners(&self) -> eyre::Result<BTreeMap<String, usize>>;

    /// Move every session `from` owns to `to`, sealed ones included. An open
    /// session on a channel where `to` already has an open session is moved
    /// to [`reassigned_channel`] (`tag` names where it came from), so both
    /// conversations survive. Idempotent: a rerun finds nothing left to move.
    fn reassign_owner(&self, from: &str, to: &str, tag: &str) -> eyre::Result<Vec<Reassigned>> {
        let mut moved = Vec::new();
        loop {
            let batch = self.list_for_owner(from, 500)?;
            if batch.is_empty() {
                return Ok(moved);
            }
            for shell in batch {
                let Some(mut session) = self.get(&shell.id)? else {
                    continue;
                };
                let renamed = !session.is_sealed() && self.find(&session.channel, to)?.is_some();
                if renamed {
                    session.channel = reassigned_channel(&session.channel, tag);
                }
                session.owner = to.to_string();
                self.save(&session)?;
                moved.push(Reassigned {
                    session_id: session.id,
                    channel: session.channel,
                    renamed,
                });
            }
        }
    }

    /// Delete a session.
    fn delete(&self, session_id: &str) -> eyre::Result<()>;

    /// Read the full transcript for a session.
    fn read_transcript(&self, session_id: &str) -> eyre::Result<Vec<TranscriptEntry>>;

    /// Export a session as a single JSON document with all messages and stats.
    fn export_session(&self, session_id: &str) -> eyre::Result<SessionExport>;

    /// Get session statistics (message count, token breakdown, tool call summary).
    fn session_stats(&self, session_id: &str) -> eyre::Result<SessionStats>;

    // ── DW-427: Session Management & Chat Enhancement Methods ─────────────

    /// Reset a session — clear all messages but retain metadata.
    fn reset_session(&self, session_id: &str) -> eyre::Result<()>;

    /// Update the per-session model override. Pass `None` to clear the override.
    fn set_session_model(&self, session_id: &str, model: Option<String>) -> eyre::Result<()>;

    /// Update the session label. Pass `None` to clear the label.
    fn set_session_label(&self, session_id: &str, label: Option<String>) -> eyre::Result<()>;

    /// Get context window usage for a session.
    fn get_context_usage(&self, session_id: &str) -> eyre::Result<SessionContextUsage>;

    /// Get all tool calls made in a session with timestamps and results.
    fn get_session_tool_calls(&self, session_id: &str) -> eyre::Result<Vec<ToolCallRecord>>;

    /// Delete a specific message from a session by its message ID.
    fn delete_message(&self, session_id: &str, message_id: &str) -> eyre::Result<()>;

    /// Full-text search across all session transcripts.
    fn search_sessions(&self, query: &str, limit: usize) -> eyre::Result<Vec<SessionSearchResult>>;

    /// Truncate a session from `message_id` (inclusive) — deletes that message and every
    /// message that follows it. Used by the retry endpoint to rewind history before
    /// re-processing. Returns an error if the session or message is not found.
    fn truncate_from_message(&self, session_id: &str, message_id: &str) -> eyre::Result<()>;

    /// Redact secrets at rest in a session's stored messages (and, for
    /// stores that keep one, its archive), reusing the ingress pattern
    /// catalogue. Idempotent. With `dry_run`, computes the stats without
    /// writing anything. A missing session yields empty stats rather than
    /// an error so a sweep can race a concurrent deletion harmlessly.
    ///
    /// Implementations MUST mutate live messages in place under their
    /// store lock (not read-clone-write) so a concurrent turn's
    /// clone-and-save can't lose messages; the worst tolerated outcome is
    /// a redaction overwritten by a racing cleartext save, caught by the
    /// next sweep.
    fn redact_secrets_in_session(
        &self,
        session_id: &str,
        dry_run: bool,
    ) -> eyre::Result<RedactionStats>;

    /// Redact secrets in the append-only archive. The default (no archive)
    /// reports zero; [`JsonlSessionStore`] overrides to rewrite the file.
    fn redact_archive_secrets(&self, _session_id: &str, _dry_run: bool) -> eyre::Result<usize> {
        Ok(0)
    }

    // ── Archive layer (WhatsApp-style display history) ──────────────────────

    /// Append a message to the append-only archive. The archive preserves all
    /// messages for display even after the LLM context is compacted.
    fn append_to_archive(&self, session_id: &str, message: &SessionMessage) -> eyre::Result<()>;

    /// Read a page of archived messages for paginated transcript display.
    /// Returns `limit` messages before `before_id` in reverse-chronological order.
    /// If `before_id` is None, returns the most recent messages.
    fn read_archive_page(
        &self,
        session_id: &str,
        before_id: Option<&str>,
        limit: usize,
    ) -> eyre::Result<ArchivePage>;

    /// One archived message, read from the same transcript `search_sessions`
    /// matches against, so every search hit can be read back in full. `None`
    /// when the session is unknown or holds no message `message_id`.
    fn read_archive_message(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> eyre::Result<Option<TranscriptEntry>>;

    /// Read a page of a *thread's* transcript, walking `prev_session_id` seams
    /// so a chain of rolled-over sessions reads as one continuous conversation.
    /// `start_session_id` is any chain member (typically the active head);
    /// the cursor is the `(before_session, before_id)` of the oldest entry the
    /// caller already holds — `before_id` belongs to `before_session`. With no
    /// cursor, returns the newest page of the thread. Entries are
    /// chronological (oldest→newest); `has_more` is true when older entries
    /// remain. Reads archives, which hold the original turns once each (the
    /// per-successor carry-over summary and verbatim copies are live-only and
    /// never archived), so no de-duplication is needed.
    fn read_thread_page(
        &self,
        start_session_id: &str,
        before_session: Option<&str>,
        before_id: Option<&str>,
        limit: usize,
    ) -> eyre::Result<ArchivePage> {
        const MAX_DEPTH: usize = 50;
        let mut cur = before_session.unwrap_or(start_session_id).to_string();
        let mut cur_before = before_id.map(|s| s.to_string());
        // Slices accumulate newest-session-first; reversed into chronological
        // order at the end (each slice is already chronological internally).
        let mut slices: Vec<Vec<TranscriptEntry>> = Vec::new();
        let mut collected = 0usize;
        let mut has_more = false;
        for _ in 0..MAX_DEPTH {
            if collected >= limit {
                has_more = true;
                break;
            }
            let page = self.read_archive_page(&cur, cur_before.as_deref(), limit - collected)?;
            collected += page.messages.len();
            let cur_has_more = page.has_more;
            slices.push(page.messages);
            if cur_has_more {
                // More older entries remain in the current session.
                has_more = true;
                break;
            }
            // Reached the start of `cur`; continue into its predecessor.
            match self
                .get(&cur)?
                .and_then(|s| s.prev_session_id().map(|p| p.to_string()))
            {
                Some(prev) if self.get(&prev)?.is_some() => {
                    cur = prev;
                    cur_before = None;
                }
                // No predecessor, or it was pruned/expired — chain ends here.
                _ => {
                    has_more = false;
                    break;
                }
            }
        }
        let mut messages = Vec::with_capacity(collected);
        for slice in slices.into_iter().rev() {
            messages.extend(slice);
        }
        Ok(ArchivePage { messages, has_more })
    }
}

/// In-memory session store (for testing).
pub struct InMemorySessionStore {
    sessions: std::sync::Mutex<std::collections::HashMap<String, Session>>,
    /// Process-local by construction: this store cannot outlive its process,
    /// so it offers no crash recovery.
    turns: std::sync::Mutex<std::collections::HashMap<String, ActiveTurn>>,
    journals: std::sync::Mutex<std::collections::HashMap<String, Vec<JournalEntry>>>,
    instance_id: String,
}

/// A turn in flight: who is running it, since when, and for which message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ActiveTurn {
    instance: String,
    started_at: DateTime<Utc>,
    message_id: Option<String>,
    /// Identifies this record specifically, not just the session it is on.
    /// Records written before this field existed deserialize to an empty
    /// token, which matches nothing and so is only ever cleared by the
    /// record's own replacement.
    #[serde(default)]
    token: String,
    #[serde(default)]
    principal: TurnPrincipal,
}

impl ActiveTurn {
    fn new(instance: &str, message_id: Option<&str>, principal: &TurnPrincipal) -> Self {
        Self {
            instance: instance.to_owned(),
            started_at: Utc::now(),
            message_id: message_id.map(str::to_string),
            token: Uuid::new_v4().to_string(),
            principal: principal.clone(),
        }
    }

    /// The record as an interrupted turn, when its owner is not `exclude`.
    fn interrupted_by_other(&self, session_id: &str, exclude: &str) -> Option<InterruptedTurn> {
        (self.instance != exclude).then(|| InterruptedTurn {
            session_id: session_id.to_string(),
            started_at: Some(self.started_at),
            message_id: self.message_id.clone(),
            token: self.token.clone(),
            principal: self.principal.clone(),
        })
    }
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self::new_as(crate::instance::process_instance_id())
    }

    /// Same store with an explicit owner identity — tests use this to model a
    /// second process, mirroring `PostgresSessionStore::with_shared_pool_as`.
    pub(crate) fn new_as(instance_id: impl Into<String>) -> Self {
        Self {
            sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
            turns: std::sync::Mutex::new(std::collections::HashMap::new()),
            journals: std::sync::Mutex::new(std::collections::HashMap::new()),
            instance_id: instance_id.into(),
        }
    }

    /// Whether `session_id` has a turn record, whichever process wrote it.
    pub fn has_turn_record(&self, session_id: &str) -> bool {
        self.turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(session_id)
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore for InMemorySessionStore {
    fn find(&self, channel: &str, owner: &str) -> eyre::Result<Option<Session>> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        // The thread's active session is the newest unsealed member sharing
        // (channel, owner); sealed sessions have rolled over and are inert.
        Ok(sessions
            .values()
            .filter(|s| s.channel == channel && s.owner == owner && !s.is_sealed())
            .max_by_key(|s| s.seq())
            .cloned())
    }

    fn get_or_create(&self, channel: &str, owner: &str, agent_id: &str) -> eyre::Result<Session> {
        if let Some(session) = self.find(channel, owner)? {
            return Ok(session);
        }
        let mut session = Session::new(channel, owner, agent_id);
        session.label = Some(format!("{}:{}", channel, owner));
        self.sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?
            .insert(session.id.clone(), session.clone());
        Ok(session)
    }

    fn save(&self, session: &Session) -> eyre::Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        sessions.insert(session.id.clone(), session.clone());
        Ok(())
    }

    fn begin_turn(
        &self,
        session_id: &str,
        message_id: Option<&str>,
        principal: &TurnPrincipal,
    ) -> eyre::Result<String> {
        let turn = ActiveTurn::new(&self.instance_id, message_id, principal);
        let token = turn.token.clone();
        self.turns
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?
            .insert(session_id.to_string(), turn);
        Ok(token)
    }

    fn end_turn(&self, session_id: &str, token: &str) -> eyre::Result<bool> {
        let mut turns = self
            .turns
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let ours = turns.get(session_id).is_some_and(|t| t.token == token);
        if ours {
            turns.remove(session_id);
        }
        Ok(ours)
    }

    fn journal_append(&self, session_id: &str, entry: &JournalEntry) -> eyre::Result<()> {
        let mut journals = self
            .journals
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let journal = journals.entry(session_id.to_string()).or_default();
        if !journal
            .iter()
            .any(|kept| kept.call == entry.call && kept.key() == entry.key())
        {
            journal.push(entry.clone());
        }
        Ok(())
    }

    fn journal(&self, session_id: &str) -> eyre::Result<Vec<JournalEntry>> {
        Ok(self
            .journals
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?
            .get(session_id)
            .cloned()
            .unwrap_or_default())
    }

    fn journal_clear(&self, session_id: &str) -> eyre::Result<()> {
        self.journals
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?
            .remove(session_id);
        Ok(())
    }

    fn journals(&self, prefix: &str) -> eyre::Result<Vec<String>> {
        Ok(self
            .journals
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?
            .iter()
            .filter(|(key, entries)| key.starts_with(prefix) && !entries.is_empty())
            .map(|(key, _)| key.clone())
            .collect())
    }

    fn merge_metadata(
        &self,
        session_id: &str,
        patch: serde_json::Map<String, serde_json::Value>,
        remove: &[&str],
    ) -> eyre::Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let Some(session) = sessions.get_mut(session_id) else {
            return Ok(());
        };
        apply_metadata_patch(&mut session.metadata, patch, remove);
        Ok(())
    }

    fn interrupted_turns(&self) -> eyre::Result<Vec<InterruptedTurn>> {
        Ok(self
            .turns
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?
            .iter()
            .filter_map(|(id, turn)| turn.interrupted_by_other(id, &self.instance_id))
            .collect())
    }

    fn get(&self, session_id: &str) -> eyre::Result<Option<Session>> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        Ok(sessions.get(session_id).cloned())
    }

    fn list(&self, channel: Option<&str>, limit: usize) -> eyre::Result<Vec<Session>> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let mut result: Vec<Session> = sessions
            .values()
            .filter(|s| channel.is_none() || Some(s.channel.as_str()) == channel)
            .cloned()
            .collect();

        result.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        result.truncate(limit);
        Ok(result)
    }

    fn owners(&self) -> eyre::Result<BTreeMap<String, usize>> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        Ok(count_owners(sessions.values()))
    }

    fn list_since(
        &self,
        agent_id: &str,
        cutoff: DateTime<Utc>,
        limit: usize,
    ) -> eyre::Result<Vec<Session>> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let mut result: Vec<Session> = sessions
            .values()
            .filter(|s| s.agent_id == agent_id && s.updated_at >= cutoff)
            .cloned()
            .collect();

        result.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        result.truncate(limit);
        Ok(result)
    }

    fn delete(&self, session_id: &str) -> eyre::Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        sessions.remove(session_id);
        self.journal_clear(session_id)
    }

    fn read_transcript(&self, session_id: &str) -> eyre::Result<Vec<TranscriptEntry>> {
        InMemorySessionStore::read_transcript(self, session_id)
    }

    fn export_session(&self, session_id: &str) -> eyre::Result<SessionExport> {
        InMemorySessionStore::export_session(self, session_id)
    }

    fn session_stats(&self, session_id: &str) -> eyre::Result<SessionStats> {
        InMemorySessionStore::session_stats(self, session_id)
    }

    fn reset_session(&self, session_id: &str) -> eyre::Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        session.messages.clear();
        session.total_tokens = 0;
        session.updated_at = chrono::Utc::now();
        Ok(())
    }

    fn set_session_model(&self, session_id: &str, model: Option<String>) -> eyre::Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        session.model_override = model;
        session.updated_at = chrono::Utc::now();
        Ok(())
    }

    fn set_session_label(&self, session_id: &str, label: Option<String>) -> eyre::Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        session.label = label;
        session.updated_at = chrono::Utc::now();
        Ok(())
    }

    fn get_context_usage(&self, session_id: &str) -> eyre::Result<SessionContextUsage> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        // NOTE: Uses the default max context window. For session-specific values, the
        // caller (gateway) should use the agent's effective CompactionConfig.max_context_tokens.
        Ok(context_usage_for(session))
    }

    fn get_session_tool_calls(&self, session_id: &str) -> eyre::Result<Vec<ToolCallRecord>> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        Ok(tool_calls_for(session))
    }

    fn redact_secrets_in_session(
        &self,
        session_id: &str,
        dry_run: bool,
    ) -> eyre::Result<RedactionStats> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let Some(session) = sessions.get_mut(session_id) else {
            return Ok(RedactionStats::default());
        };
        if dry_run {
            let mut probe = session.messages.clone();
            Ok(redact_messages_in_place(&mut probe))
        } else {
            Ok(redact_messages_in_place(&mut session.messages))
        }
    }

    fn delete_message(&self, session_id: &str, message_id: &str) -> eyre::Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        let before = session.messages.len();
        let removed_tokens: u32 = session
            .messages
            .iter()
            .filter(|m| m.id == message_id)
            .map(|m| m.tokens)
            .sum();
        session.messages.retain(|m| m.id != message_id);
        if session.messages.len() == before {
            return Err(eyre::eyre!("Message not found: {message_id}"));
        }
        session.total_tokens = session.total_tokens.saturating_sub(removed_tokens as u64);
        session.updated_at = chrono::Utc::now();
        Ok(())
    }

    fn search_sessions(&self, query: &str, limit: usize) -> eyre::Result<Vec<SessionSearchResult>> {
        let terms = search_terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let single_term = terms.len() == 1;
        let mut scored: Vec<(usize, SessionSearchResult)> = Vec::new();
        'outer: for session in sessions.values() {
            for (i, msg) in session.messages.iter().enumerate() {
                if single_term && scored.len() >= limit {
                    break 'outer;
                }
                let score = coordination_score(&msg.content.to_lowercase(), &terms);
                if score > 0 {
                    scored.push((
                        score,
                        SessionSearchResult {
                            session_id: session.id.clone(),
                            label: session.label.clone(),
                            message_id: msg.id.clone(),
                            role: msg.role.clone(),
                            excerpt: format_search_excerpt(&session.messages, i),
                            timestamp: msg.timestamp,
                        },
                    ));
                }
            }
        }
        Ok(rank_search_results(scored, limit))
    }

    fn truncate_from_message(&self, session_id: &str, message_id: &str) -> eyre::Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        // Find the index of the target message.
        let start_idx = session
            .messages
            .iter()
            .position(|m| m.id == message_id)
            .ok_or_else(|| eyre::eyre!("Message not found: {message_id}"))?;
        // Remove that message and everything after it.
        let removed: Vec<_> = session.messages.drain(start_idx..).collect();
        let removed_tokens: u32 = removed.iter().map(|m| m.tokens).sum();
        session.total_tokens = session.total_tokens.saturating_sub(removed_tokens as u64);
        session.updated_at = chrono::Utc::now();
        Ok(())
    }

    fn append_to_archive(&self, _session_id: &str, _message: &SessionMessage) -> eyre::Result<()> {
        Ok(())
    }

    fn read_archive_page(
        &self,
        session_id: &str,
        before_id: Option<&str>,
        limit: usize,
    ) -> eyre::Result<ArchivePage> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;

        let entries: Vec<TranscriptEntry> = session
            .messages
            .iter()
            .map(|msg| message_to_transcript_entry(session_id, msg))
            .collect();

        paginate_entries(entries, before_id, limit)
    }

    fn read_archive_message(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> eyre::Result<Option<TranscriptEntry>> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
        Ok(sessions
            .get(session_id)
            .and_then(|session| session.messages.iter().find(|m| m.id == message_id))
            .map(|msg| message_to_transcript_entry(session_id, msg)))
    }
}

// ─── Transcript Operations for InMemorySessionStore ───

impl InMemorySessionStore {
    /// Read the full transcript for a session as rich `TranscriptEntry` items.
    pub fn read_transcript(&self, session_id: &str) -> eyre::Result<Vec<TranscriptEntry>> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let session = sessions
            .get(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;

        Ok(session
            .messages
            .iter()
            .map(|msg| message_to_transcript_entry(session_id, msg))
            .collect())
    }

    /// Export a session as a single JSON document with all messages and stats.
    pub fn export_session(&self, session_id: &str) -> eyre::Result<SessionExport> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let session = sessions
            .get(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;

        let transcript: Vec<TranscriptEntry> = session
            .messages
            .iter()
            .map(|msg| message_to_transcript_entry(session_id, msg))
            .collect();

        let stats = compute_stats(&transcript, session);

        Ok(SessionExport {
            id: session.id.clone(),
            label: session.label.clone(),
            channel: session.channel.clone(),
            owner: session.owner.clone(),
            agent_id: session.agent_id.clone(),
            kind: session.kind.clone(),
            created_at: session.created_at,
            updated_at: session.updated_at,
            total_tokens: session.total_tokens,
            model_override: session.model_override.clone(),
            transcript,
            stats,
        })
    }

    /// Get session statistics.
    pub fn session_stats(&self, session_id: &str) -> eyre::Result<SessionStats> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let session = sessions
            .get(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;

        let transcript: Vec<TranscriptEntry> = session
            .messages
            .iter()
            .map(|msg| message_to_transcript_entry(session_id, msg))
            .collect();

        Ok(compute_stats(&transcript, session))
    }
}

// ─── Session Metadata (stored in .meta.json) ───

/// Metadata for a session, stored separately from messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMeta {
    id: String,
    label: Option<String>,
    channel: String,
    owner: String,
    agent_id: String,
    kind: SessionKind,
    #[serde(default)]
    context_type: Option<SessionContextType>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    metadata: serde_json::Value,
    total_tokens: u64,
    #[serde(default)]
    compaction_history: Vec<CompactionEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_override: Option<String>,
    /// Cached count of messages in the corresponding `.jsonl` file.
    /// Lets the lazy loader hand callers an unhydrated session shell
    /// while still answering `message_count()` correctly. Old meta
    /// files without this field default to 0; the loader recovers the
    /// real count by counting JSONL lines on first read.
    #[serde(default)]
    message_count: usize,
}

impl SessionMeta {
    fn from_session(session: &Session) -> Self {
        Self {
            id: session.id.clone(),
            label: session.label.clone(),
            channel: session.channel.clone(),
            owner: session.owner.clone(),
            agent_id: session.agent_id.clone(),
            kind: session.kind.clone(),
            context_type: session.context_type.clone(),
            created_at: session.created_at,
            updated_at: session.updated_at,
            metadata: session.metadata.clone(),
            total_tokens: session.total_tokens,
            compaction_history: session.compaction_history.clone(),
            model_override: session.model_override.clone(),
            message_count: session.message_count(),
        }
    }

    fn into_session(self, messages: Vec<SessionMessage>) -> Session {
        // Empty-on-disk sessions don't need a shell marker — there's
        // nothing to hydrate. `Some(0)` would just trigger a no-op
        // round-trip through `hydrate_in_cache` on first access.
        let cached_message_count = (self.message_count > 0).then_some(self.message_count);
        Session {
            id: self.id,
            label: self.label,
            channel: self.channel,
            owner: self.owner,
            agent_id: self.agent_id,
            kind: self.kind,
            context_type: self.context_type,
            messages,
            created_at: self.created_at,
            updated_at: self.updated_at,
            metadata: self.metadata,
            total_tokens: self.total_tokens,
            compaction_history: self.compaction_history,
            model_override: self.model_override,
            cached_message_count,
            loaded_revision: None,
        }
    }
}

// ─── JSONL Session Store ───

/// Disk-backed session store using JSONL files.
///
/// Layout:
///   `{base_dir}/sessions/{session_id}.jsonl`      — one JSON line per message
///   `{base_dir}/sessions/{session_id}.meta.json`   — session metadata
///
/// An in-memory cache is maintained for fast lookups. All writes go to both
/// the cache and disk. On construction, existing sessions are loaded from disk.
pub struct JsonlSessionStore {
    base_dir: PathBuf,
    cache: std::sync::Mutex<HashMap<String, Session>>,
    instance_id: String,
}

/// Snapshot of in-memory session cache size, for memory observability.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionCacheStats {
    pub sessions: u64,
    pub messages: u64,
    /// Sum of `serde_json::to_vec(msg).len()` across all cached messages —
    /// includes `content`, `tool_data` JSON, and attachment metadata.
    /// Computed by re-serialising each cached message at snapshot time
    /// (O(messages) work per call). Not the exact on-heap footprint —
    /// JSON has overhead vs. native structs and attachment payloads
    /// stored elsewhere — but a useful upper-bound proxy for tool-heavy
    /// agents where tool outputs dominate over plain content.
    pub estimated_bytes: u64,
}

impl JsonlSessionStore {
    /// Create a new JSONL session store, loading any existing sessions from disk.
    pub fn new(data_dir: &Path) -> eyre::Result<Self> {
        Self::new_as(data_dir, crate::instance::process_instance_id())
    }

    /// Same store with an explicit owner identity — tests use this to model a
    /// successor process, mirroring `PostgresSessionStore::with_shared_pool_as`.
    pub(crate) fn new_as(data_dir: &Path, instance_id: impl Into<String>) -> eyre::Result<Self> {
        let base_dir = data_dir.join("sessions");
        crate::private_fs::ensure_private_dir(&base_dir)?;

        let store = Self {
            base_dir,
            cache: std::sync::Mutex::new(HashMap::new()),
            instance_id: instance_id.into(),
        };

        store.load_all_from_disk()?;
        Ok(store)
    }

    /// Snapshot the in-memory cache for memory observability. Walks every
    /// cached message and re-serialises it to JSON to estimate bytes —
    /// this catches tool_data and attachment metadata that a content-only
    /// counter would miss (significant for tool-heavy agents). Cost is
    /// O(messages) per call; with the 60s default snapshot interval this
    /// is well under 1% of one core in the workloads we expect.
    pub fn cache_stats(&self) -> SessionCacheStats {
        let Ok(cache) = self.cache.lock() else {
            return SessionCacheStats::default();
        };
        let mut stats = SessionCacheStats {
            sessions: cache.len() as u64,
            ..Default::default()
        };
        for session in cache.values() {
            stats.messages += session.messages.len() as u64;
            for msg in &session.messages {
                stats.estimated_bytes +=
                    serde_json::to_vec(msg).map(|v| v.len() as u64).unwrap_or(0);
            }
        }
        stats
    }

    /// Path to the JSONL messages file for a session.
    fn messages_path(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(format!("{session_id}.jsonl"))
    }

    /// Path to the metadata JSON file for a session.
    fn meta_path(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(format!("{session_id}.meta.json"))
    }

    /// Path to the append-only archive file for display history.
    fn archive_path(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(format!("{session_id}.archive.jsonl"))
    }

    /// Path to the sidecar recording a turn in flight — outside the session
    /// files because `write_to_disk` replaces them wholesale.
    fn turn_path(&self, session_id: &str) -> PathBuf {
        self.base_dir
            .join(format!("{session_id}{TURN_RECORD_SUFFIX}"))
    }

    fn journal_path(&self, session_id: &str) -> PathBuf {
        self.base_dir
            .join(format!("{session_id}{TURN_JOURNAL_SUFFIX}"))
    }

    /// Every turn sidecar on disk, keyed by session id. An unreadable record —
    /// a sidecar truncated by a crash mid-write — is unlinked rather than
    /// skipped: nothing can act on it, and leaving it re-warns on every sweep
    /// forever.
    fn read_turn_records(&self) -> eyre::Result<HashMap<String, ActiveTurn>> {
        let entries = match std::fs::read_dir(&self.base_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(e) => return Err(eyre::eyre!("Failed to read sessions dir: {e}")),
        };
        Ok(entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let session_id = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(TURN_RECORD_SUFFIX))?
                    .to_string();
                match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| {
                    serde_json::from_slice::<ActiveTurn>(&b).map_err(|e| e.to_string())
                }) {
                    Ok(turn) => Some((session_id, turn)),
                    Err(e) => {
                        warn!(error = %e, path = %path.display(), "Discarding unreadable turn record");
                        let _ = std::fs::remove_file(&path);
                        None
                    }
                }
            })
            .collect())
    }

    /// Write the full session to disk (metadata + JSONL). Refuses to
    /// rewrite the JSONL when the session is a shell from `list()` —
    /// otherwise a save-after-list bug would silently truncate live data.
    fn write_to_disk(&self, session: &Session) -> eyre::Result<()> {
        let meta = SessionMeta::from_session(session);
        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|e| eyre::eyre!("Failed to serialize session metadata: {e}"))?;
        atomic_write(&self.meta_path(&session.id), meta_json.as_bytes())?;

        if let Some(cached) = session.cached_message_count {
            warn!(
                session_id = %session.id,
                cached_count = cached,
                "Refusing to overwrite JSONL from an unhydrated session shell — \
                 on-disk messages preserved. Caller should hydrate before save."
            );
            return Ok(());
        }

        let mut buf = Vec::new();
        for msg in &session.messages {
            let line = serde_json::to_string(msg)
                .map_err(|e| eyre::eyre!("Failed to serialize message: {e}"))?;
            buf.extend_from_slice(line.as_bytes());
            buf.push(b'\n');
        }
        atomic_write(&self.messages_path(&session.id), &buf)?;

        Ok(())
    }

    /// Load only the metadata for a session (no message bodies). Returns
    /// `Ok(None)` when the meta file is missing. Recovers `message_count`
    /// from JSONL line count when the meta file pre-dates that field.
    fn load_meta(&self, session_id: &str) -> eyre::Result<Option<SessionMeta>> {
        let meta_path = self.meta_path(session_id);
        if !meta_path.exists() {
            return Ok(None);
        }
        let meta_content = std::fs::read_to_string(&meta_path)
            .map_err(|e| eyre::eyre!("Failed to read {}: {e}", meta_path.display()))?;
        let mut meta: SessionMeta = serde_json::from_str(&meta_content)
            .map_err(|e| eyre::eyre!("Failed to parse {}: {e}", meta_path.display()))?;
        if meta.message_count == 0 {
            meta.message_count = self.count_jsonl_lines(session_id);
        }
        Ok(Some(meta))
    }

    /// Count the lines in a session's JSONL file (sequential read, no
    /// JSON parsing). Returns 0 if the file is missing or unreadable.
    fn count_jsonl_lines(&self, session_id: &str) -> usize {
        let path = self.messages_path(session_id);
        let Ok(content) = std::fs::read(&path) else {
            return 0;
        };
        let newlines = content.iter().filter(|&&b| b == b'\n').count();
        if content.is_empty() {
            0
        } else if content.last() == Some(&b'\n') {
            newlines
        } else {
            newlines + 1
        }
    }

    /// Hydrate a cached session shell — read its messages from disk and
    /// populate them in place. No-op if the session is already hydrated
    /// (no shell marker) or absent from the cache. Always clears the
    /// shell marker on exit so subsequent writes know `messages` is
    /// authoritative.
    fn hydrate_in_cache(&self, session_id: &str) -> eyre::Result<()> {
        let needs_load = {
            let cache = self
                .cache
                .lock()
                .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
            cache
                .get(session_id)
                .is_some_and(|s| s.cached_message_count.is_some())
        };
        if !needs_load {
            return Ok(());
        }
        let messages = self.read_messages(session_id)?;
        let mut cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
        if let Some(s) = cache.get_mut(session_id) {
            if s.messages.is_empty() {
                s.messages = messages;
            }
            s.cached_message_count = None;
        }
        Ok(())
    }

    /// Read messages from a JSONL file.
    fn read_messages(&self, session_id: &str) -> eyre::Result<Vec<SessionMessage>> {
        let path = self.messages_path(session_id);
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = std::fs::File::open(&path)
            .map_err(|e| eyre::eyre!("Failed to open {}: {e}", path.display()))?;
        let reader = std::io::BufReader::new(file);
        let mut messages = Vec::new();

        for (line_num, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| {
                eyre::eyre!(
                    "Failed to read line {} of {}: {e}",
                    line_num + 1,
                    path.display()
                )
            })?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionMessage>(trimmed) {
                Ok(msg) => messages.push(msg),
                Err(e) => {
                    warn!(
                        session_id = session_id,
                        line = line_num + 1,
                        error = %e,
                        "Skipping malformed JSONL line"
                    );
                }
            }
        }

        Ok(messages)
    }

    /// Read the full archived transcript for a session — every message ever
    /// recorded, including ones later compacted out of the live session.
    /// Falls back to the live messages when no archive exists yet. This is the
    /// source for `search_sessions` so drilling into original detail survives
    /// compaction and rollover.
    fn read_archive_messages(&self, session_id: &str) -> eyre::Result<Vec<SessionMessage>> {
        let path = self.archive_path(session_id);
        if !path.exists() {
            return self.read_messages(session_id);
        }
        let file = std::fs::File::open(&path)
            .map_err(|e| eyre::eyre!("Failed to open archive {}: {e}", path.display()))?;
        let mut messages = Vec::new();
        for (line_num, line) in std::io::BufReader::new(file).lines().enumerate() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionMessage>(trimmed) {
                Ok(msg) => messages.push(msg),
                Err(e) => warn!(
                    session_id = session_id,
                    line = line_num + 1,
                    error = %e,
                    "Skipping malformed archive line"
                ),
            }
        }
        Ok(messages)
    }

    /// Scan the sessions directory and populate the cache with session
    /// shells — metadata only, `messages` empty. Bodies hydrate lazily
    /// via [`Self::hydrate_in_cache`] on first read.
    fn load_all_from_disk(&self) -> eyre::Result<()> {
        let mut loaded = 0u32;
        let mut errors = 0u32;

        let entries = match std::fs::read_dir(&self.base_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(eyre::eyre!("Failed to read sessions dir: {e}")),
        };

        let mut cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if !file_name.ends_with(".meta.json") {
                continue;
            }

            let session_id = file_name.trim_end_matches(".meta.json");
            // Drop the cache lock for the disk reads that don't need it.
            drop(cache);

            let archive = self.archive_path(session_id);
            if !archive.exists() {
                let context = self.messages_path(session_id);
                if context.exists() {
                    if let Err(e) = std::fs::copy(&context, &archive) {
                        warn!(session_id, error = %e, "Failed to seed archive from context JSONL");
                    }
                }
            }

            match self.load_meta(session_id) {
                Ok(Some(meta)) => {
                    let shell = meta.into_session(Vec::new());
                    cache = self
                        .cache
                        .lock()
                        .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
                    debug!(
                        session_id = session_id,
                        message_count = shell.cached_message_count.unwrap_or(0),
                        "Loaded session shell (lazy)"
                    );
                    cache.insert(shell.id.clone(), shell);
                    loaded += 1;
                }
                Ok(None) => {
                    cache = self
                        .cache
                        .lock()
                        .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
                }
                Err(e) => {
                    warn!(session_id = session_id, error = %e, "Failed to load session, skipping");
                    errors += 1;
                    cache = self
                        .cache
                        .lock()
                        .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
                }
            }
        }

        if loaded > 0 || errors > 0 {
            info!(loaded, errors, "Session store initialized from disk");
        }

        Ok(())
    }

    /// Remove session files from disk.
    fn remove_from_disk(&self, session_id: &str) -> eyre::Result<()> {
        let meta_path = self.meta_path(session_id);
        let messages_path = self.messages_path(session_id);

        if meta_path.exists() {
            std::fs::remove_file(&meta_path)
                .map_err(|e| eyre::eyre!("Failed to remove {}: {e}", meta_path.display()))?;
        }
        if messages_path.exists() {
            std::fs::remove_file(&messages_path)
                .map_err(|e| eyre::eyre!("Failed to remove {}: {e}", messages_path.display()))?;
        }
        for sidecar in [self.turn_path(session_id), self.journal_path(session_id)] {
            if sidecar.exists() {
                std::fs::remove_file(&sidecar)
                    .map_err(|e| eyre::eyre!("Failed to remove {}: {e}", sidecar.display()))?;
            }
        }

        Ok(())
    }
}

impl SessionStore for JsonlSessionStore {
    fn cache_stats(&self) -> SessionCacheStats {
        // Delegate to the inherent implementation that walks the cache.
        Self::cache_stats(self)
    }

    fn find(&self, channel: &str, owner: &str) -> eyre::Result<Option<Session>> {
        // Resolve the id under the lock, then go through `get()` so the
        // shell gets hydrated before the caller sees it.
        let existing_id = {
            let cache = self
                .cache
                .lock()
                .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
            cache
                .values()
                .filter(|s| s.channel == channel && s.owner == owner && !s.is_sealed())
                .max_by_key(|s| s.seq())
                .map(|s| s.id.clone())
        };
        match existing_id {
            Some(id) => self.get(&id),
            None => Ok(None),
        }
    }

    fn get_or_create(&self, channel: &str, owner: &str, agent_id: &str) -> eyre::Result<Session> {
        if let Some(session) = self.find(channel, owner)? {
            return Ok(session);
        }

        let key = format!("{channel}:{owner}");
        let mut session = Session::new(channel, owner, agent_id);
        session.label = Some(key);

        self.write_to_disk(&session)?;

        let mut cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
        cache.insert(session.id.clone(), session.clone());
        Ok(session)
    }

    fn save(&self, session: &Session) -> eyre::Result<()> {
        self.write_to_disk(session)?;

        let mut cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
        cache.insert(session.id.clone(), session.clone());
        Ok(())
    }

    fn begin_turn(
        &self,
        session_id: &str,
        message_id: Option<&str>,
        principal: &TurnPrincipal,
    ) -> eyre::Result<String> {
        let turn = ActiveTurn::new(&self.instance_id, message_id, principal);
        let record = serde_json::to_vec(&turn)
            .map_err(|e| eyre::eyre!("Failed to serialize turn record: {e}"))?;
        atomic_write(&self.turn_path(session_id), &record)?;
        Ok(turn.token)
    }

    fn end_turn(&self, session_id: &str, token: &str) -> eyre::Result<bool> {
        let path = self.turn_path(session_id);
        match std::fs::read(&path) {
            Ok(bytes) => {
                if serde_json::from_slice::<ActiveTurn>(&bytes)
                    .is_ok_and(|turn| turn.token != token)
                {
                    return Ok(false);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(eyre::eyre!("Failed to read turn record: {e}")),
        }
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(eyre::eyre!("Failed to remove turn record: {e}")),
        }
    }

    /// One JSON line per entry, flushed to disk before returning. A line cut
    /// short by a crash is closed off before the next entry, so it costs only
    /// itself and is skipped when read; a repeated entry is dropped then, so
    /// appending needs no read first.
    fn journal_append(&self, session_id: &str, entry: &JournalEntry) -> eyre::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.journal_path(session_id);
        let created = !path.exists();
        let mut file = crate::private_fs::private_file_options()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|e| eyre::eyre!("Failed to open turn journal: {e}"))?;
        let mut last = [b'\n'];
        if file.metadata().map(|m| m.len()).unwrap_or(0) > 0 {
            file.seek(SeekFrom::End(-1))
                .and_then(|_| file.read_exact(&mut last))
                .map_err(|e| eyre::eyre!("Failed to read turn journal: {e}"))?;
        }
        let mut line = match last[0] {
            b'\n' => Vec::new(),
            _ => vec![b'\n'],
        };
        line.extend(
            serde_json::to_vec(entry)
                .map_err(|e| eyre::eyre!("Failed to serialize journal entry: {e}"))?,
        );
        line.push(b'\n');
        file.write_all(&line)
            .and_then(|()| file.sync_data())
            .map_err(|e| eyre::eyre!("Failed to append to turn journal: {e}"))?;
        if created {
            std::fs::File::open(&self.base_dir)
                .and_then(|dir| dir.sync_all())
                .map_err(|e| eyre::eyre!("Failed to sync the sessions dir: {e}"))?;
        }
        Ok(())
    }

    fn journal(&self, session_id: &str) -> eyre::Result<Vec<JournalEntry>> {
        let bytes = match std::fs::read(self.journal_path(session_id)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(eyre::eyre!("Failed to read turn journal: {e}")),
        };
        Ok(first_of_each(
            bytes
                .split(|b| *b == b'\n')
                .filter_map(|line| serde_json::from_slice(line).ok()),
        ))
    }

    fn journal_clear(&self, session_id: &str) -> eyre::Result<()> {
        match std::fs::remove_file(self.journal_path(session_id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(eyre::eyre!("Failed to clear turn journal: {e}")),
        }
    }

    fn journals(&self, prefix: &str) -> eyre::Result<Vec<String>> {
        let entries = match std::fs::read_dir(&self.base_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(eyre::eyre!("Failed to read sessions dir: {e}")),
        };
        Ok(entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()?
                    .strip_suffix(TURN_JOURNAL_SUFFIX)
                    .filter(|key| key.starts_with(prefix))
                    .map(String::from)
            })
            .collect())
    }

    fn merge_metadata(
        &self,
        session_id: &str,
        patch: serde_json::Map<String, serde_json::Value>,
        remove: &[&str],
    ) -> eyre::Result<()> {
        let Some(mut session) = self.get(session_id)? else {
            return Ok(());
        };
        apply_metadata_patch(&mut session.metadata, patch, remove);
        self.save(&session)
    }

    fn interrupted_turns(&self) -> eyre::Result<Vec<InterruptedTurn>> {
        Ok(self
            .read_turn_records()?
            .iter()
            .filter_map(|(id, turn)| turn.interrupted_by_other(id, &self.instance_id))
            .collect())
    }

    fn get(&self, session_id: &str) -> eyre::Result<Option<Session>> {
        self.hydrate_in_cache(session_id)?;
        let cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
        Ok(cache.get(session_id).cloned())
    }

    /// List sessions. Entries may be shells (`messages` empty, but
    /// `message_count()` still answers correctly) — callers that need
    /// the bodies should `get(id)` per session, which hydrates lazily.
    /// Treat the returned sessions as read-only; mutating one and
    /// passing it to `save` triggers the shell-protection path in
    /// `write_to_disk` to preserve the on-disk JSONL.
    fn list(&self, channel: Option<&str>, limit: usize) -> eyre::Result<Vec<Session>> {
        let cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;

        let mut result: Vec<Session> = cache
            .values()
            .filter(|s| channel.is_none_or(|ch| s.channel == ch))
            .cloned()
            .collect();

        result.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        result.truncate(limit);
        Ok(result)
    }

    fn list_since(
        &self,
        agent_id: &str,
        cutoff: DateTime<Utc>,
        limit: usize,
    ) -> eyre::Result<Vec<Session>> {
        let cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;

        let mut result: Vec<Session> = cache
            .values()
            .filter(|s| s.agent_id == agent_id && s.updated_at >= cutoff)
            .cloned()
            .collect();

        result.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        result.truncate(limit);
        Ok(result)
    }

    fn owners(&self) -> eyre::Result<BTreeMap<String, usize>> {
        let cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
        Ok(count_owners(cache.values()))
    }

    /// Overrides the trait default (`list(None, 10_000)` + filter), which
    /// clones every cached session — including hydrated ones with full
    /// message vectors — before filtering. Filtering by `owner` before
    /// `cloned()` keeps the per-call cost to cheap clones of one owner's
    /// sessions.
    fn list_for_owner(&self, owner: &str, limit: usize) -> eyre::Result<Vec<Session>> {
        let cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;

        let mut result: Vec<Session> = cache
            .values()
            .filter(|s| s.owner == owner)
            .cloned()
            .collect();

        result.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        result.truncate(limit);
        Ok(result)
    }

    fn delete(&self, session_id: &str) -> eyre::Result<()> {
        self.remove_from_disk(session_id)?;

        let mut cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
        cache.remove(session_id);
        Ok(())
    }

    fn read_transcript(&self, session_id: &str) -> eyre::Result<Vec<TranscriptEntry>> {
        JsonlSessionStore::read_transcript(self, session_id)
    }

    fn export_session(&self, session_id: &str) -> eyre::Result<SessionExport> {
        JsonlSessionStore::export_session(self, session_id)
    }

    fn session_stats(&self, session_id: &str) -> eyre::Result<SessionStats> {
        JsonlSessionStore::session_stats(self, session_id)
    }

    fn reset_session(&self, session_id: &str) -> eyre::Result<()> {
        // Reset writes an empty JSONL, so we deliberately skip hydration.
        // Clearing the shell marker is what tells `write_to_disk` that
        // the empty `messages` is intentional, not an unhydrated shell.
        let mut cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
        let session = cache
            .get_mut(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        session.messages.clear();
        session.cached_message_count = None;
        session.total_tokens = 0;
        session.updated_at = chrono::Utc::now();
        let session_clone = session.clone();
        drop(cache);
        self.write_to_disk(&session_clone)
    }

    fn set_session_model(&self, session_id: &str, model: Option<String>) -> eyre::Result<()> {
        self.hydrate_in_cache(session_id)?;
        let mut cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
        let session = cache
            .get_mut(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        session.model_override = model;
        session.updated_at = chrono::Utc::now();
        let session_clone = session.clone();
        drop(cache);
        self.write_to_disk(&session_clone)
    }

    fn set_session_label(&self, session_id: &str, label: Option<String>) -> eyre::Result<()> {
        self.hydrate_in_cache(session_id)?;
        let mut cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
        let session = cache
            .get_mut(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        session.label = label;
        session.updated_at = chrono::Utc::now();
        let session_clone = session.clone();
        drop(cache);
        self.write_to_disk(&session_clone)
    }

    fn get_context_usage(&self, session_id: &str) -> eyre::Result<SessionContextUsage> {
        let session = self
            .get(session_id)?
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        // NOTE: Uses the default max context window. For session-specific values, the
        // caller (gateway) should use the agent's effective CompactionConfig.max_context_tokens.
        Ok(context_usage_for(&session))
    }

    fn get_session_tool_calls(&self, session_id: &str) -> eyre::Result<Vec<ToolCallRecord>> {
        let session = self
            .get(session_id)?
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        Ok(tool_calls_for(&session))
    }

    fn redact_secrets_in_session(
        &self,
        session_id: &str,
        dry_run: bool,
    ) -> eyre::Result<RedactionStats> {
        self.hydrate_in_cache(session_id)?;
        let (mut stats, snapshot) = {
            let mut cache = self
                .cache
                .lock()
                .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
            match cache.get_mut(session_id) {
                Some(session) if dry_run => {
                    let mut probe = session.messages.clone();
                    (redact_messages_in_place(&mut probe), None)
                }
                Some(session) => {
                    let stats = redact_messages_in_place(&mut session.messages);
                    // Snapshot under the lock; write after releasing it
                    // (mirrors delete_message) so disk reflects the same
                    // in-place mutation the cache now holds.
                    let snapshot = (stats.messages_redacted > 0).then(|| session.clone());
                    (stats, snapshot)
                }
                None => (RedactionStats::default(), None),
            }
        };
        if let Some(session) = snapshot {
            self.write_to_disk(&session)?;
        }
        stats.archive_messages_redacted = self.redact_archive_secrets(session_id, dry_run)?;
        Ok(stats)
    }

    fn delete_message(&self, session_id: &str, message_id: &str) -> eyre::Result<()> {
        self.hydrate_in_cache(session_id)?;
        let mut cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
        let session = cache
            .get_mut(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        let before = session.messages.len();
        let removed_tokens: u32 = session
            .messages
            .iter()
            .filter(|m| m.id == message_id)
            .map(|m| m.tokens)
            .sum();
        session.messages.retain(|m| m.id != message_id);
        if session.messages.len() == before {
            return Err(eyre::eyre!("Message not found: {message_id}"));
        }
        session.total_tokens = session.total_tokens.saturating_sub(removed_tokens as u64);
        session.updated_at = chrono::Utc::now();
        let session_clone = session.clone();
        drop(cache);
        self.write_to_disk(&session_clone)
    }

    fn search_sessions(&self, query: &str, limit: usize) -> eyre::Result<Vec<SessionSearchResult>> {
        let terms = search_terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        // Snapshot (id, label) under the lock, newest first. Bodies are read
        // from each session's ARCHIVE below — the full transcript that
        // survives compaction/rollover — not the compacted live messages, so
        // the agent can drill into original detail it no longer holds in
        // context.
        let candidates: Vec<(String, Option<String>)> = {
            let cache = self
                .cache
                .lock()
                .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
            let mut rows: Vec<(String, Option<String>, DateTime<Utc>)> = cache
                .values()
                .map(|s| (s.id.clone(), s.label.clone(), s.updated_at))
                .collect();
            rows.sort_by(|a, b| b.2.cmp(&a.2));
            rows.into_iter().map(|(id, label, _)| (id, label)).collect()
        };

        // A single-term query is already correctly ordered by the newest-first
        // candidate sweep, so it keeps the early-exit at `limit`. A multi-word
        // query needs every match scored before ranking, so it scans the full
        // set.
        let single_term = terms.len() == 1;
        let mut scored: Vec<(usize, SessionSearchResult)> = Vec::new();
        for (id, label) in &candidates {
            if single_term && scored.len() >= limit {
                break;
            }
            let messages = self.read_archive_messages(id)?;
            for (i, msg) in messages.iter().enumerate() {
                if single_term && scored.len() >= limit {
                    break;
                }
                let score = coordination_score(&msg.content.to_lowercase(), &terms);
                if score > 0 {
                    scored.push((
                        score,
                        SessionSearchResult {
                            session_id: id.clone(),
                            label: label.clone(),
                            message_id: msg.id.clone(),
                            role: msg.role.clone(),
                            excerpt: format_search_excerpt(&messages, i),
                            timestamp: msg.timestamp,
                        },
                    ));
                }
            }
        }
        Ok(rank_search_results(scored, limit))
    }

    fn truncate_from_message(&self, session_id: &str, message_id: &str) -> eyre::Result<()> {
        self.hydrate_in_cache(session_id)?;
        let mut cache = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
        let session = cache
            .get_mut(session_id)
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        let start_idx = session
            .messages
            .iter()
            .position(|m| m.id == message_id)
            .ok_or_else(|| eyre::eyre!("Message not found: {message_id}"))?;
        let removed: Vec<_> = session.messages.drain(start_idx..).collect();
        let removed_tokens: u32 = removed.iter().map(|m| m.tokens).sum();
        session.total_tokens = session.total_tokens.saturating_sub(removed_tokens as u64);
        session.updated_at = chrono::Utc::now();
        let session_clone = session.clone();
        drop(cache);
        self.write_to_disk(&session_clone)
    }

    fn append_to_archive(&self, session_id: &str, message: &SessionMessage) -> eyre::Result<()> {
        let path = self.archive_path(session_id);
        let mut file = crate::private_fs::private_file_options()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| eyre::eyre!("Failed to open archive {}: {e}", path.display()))?;
        let line = serde_json::to_string(message)
            .map_err(|e| eyre::eyre!("Failed to serialize archive message: {e}"))?;
        writeln!(file, "{line}")
            .map_err(|e| eyre::eyre!("Failed to write to archive {}: {e}", path.display()))?;
        Ok(())
    }

    fn redact_archive_secrets(&self, session_id: &str, dry_run: bool) -> eyre::Result<usize> {
        let path = self.archive_path(session_id);
        if !path.exists() {
            return Ok(0);
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| eyre::eyre!("Failed to read archive {}: {e}", path.display()))?;
        let mut redacted = 0usize;
        // Built only when applying; a dry run parses + counts without
        // re-serializing the archive.
        let mut out = String::new();
        for line in content.lines() {
            if line.is_empty() {
                continue;
            }
            // Parse each line back to a message so `tool_data` leaves are
            // walked too; a malformed line is still scrubbed as raw text
            // rather than skipped.
            match serde_json::from_str::<SessionMessage>(line) {
                Ok(mut msg) => {
                    if redact_message_secrets(&mut msg) {
                        redacted += 1;
                    }
                    if !dry_run {
                        out.push_str(&serde_json::to_string(&msg).map_err(|e| {
                            eyre::eyre!("Failed to serialize archive message: {e}")
                        })?);
                        out.push('\n');
                    }
                }
                Err(_) => {
                    let scrubbed = crate::redact::redact_secrets(line);
                    if scrubbed != line {
                        redacted += 1;
                    }
                    if !dry_run {
                        out.push_str(&scrubbed);
                        out.push('\n');
                    }
                }
            }
        }
        if redacted > 0 && !dry_run {
            atomic_write(&path, out.as_bytes())?;
        }
        Ok(redacted)
    }

    fn read_archive_page(
        &self,
        session_id: &str,
        before_id: Option<&str>,
        limit: usize,
    ) -> eyre::Result<ArchivePage> {
        let path = self.archive_path(session_id);
        if !path.exists() {
            // Fall back to context JSONL for sessions without an archive yet
            let messages = self.read_messages(session_id)?;
            let entries: Vec<TranscriptEntry> = messages
                .iter()
                .map(|msg| message_to_transcript_entry(session_id, msg))
                .collect();
            return paginate_entries(entries, before_id, limit);
        }

        let file = std::fs::File::open(&path)
            .map_err(|e| eyre::eyre!("Failed to open archive {}: {e}", path.display()))?;
        let reader = std::io::BufReader::new(file);
        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<SessionMessage>(trimmed) {
                entries.push(message_to_transcript_entry(session_id, &msg));
            }
        }
        paginate_entries(entries, before_id, limit)
    }

    fn read_archive_message(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> eyre::Result<Option<TranscriptEntry>> {
        let known = self
            .cache
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?
            .contains_key(session_id);
        if !known {
            return Ok(None);
        }
        Ok(self
            .read_archive_messages(session_id)?
            .iter()
            .find(|m| m.id == message_id)
            .map(|msg| message_to_transcript_entry(session_id, msg)))
    }
}

// ─── Transcript Operations ───

impl JsonlSessionStore {
    /// Read the full transcript for a session as rich `TranscriptEntry` items.
    ///
    /// Converts stored `SessionMessage`s into `TranscriptEntry`s, extracting
    /// tool metadata and token usage where available.
    pub fn read_transcript(&self, session_id: &str) -> eyre::Result<Vec<TranscriptEntry>> {
        let session = self
            .get(session_id)?
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;

        Ok(session
            .messages
            .iter()
            .map(|msg| message_to_transcript_entry(session_id, msg))
            .collect())
    }

    /// Export a session as a single JSON document with all messages and stats.
    pub fn export_session(&self, session_id: &str) -> eyre::Result<SessionExport> {
        let session = self
            .get(session_id)?
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;

        let transcript: Vec<TranscriptEntry> = session
            .messages
            .iter()
            .map(|msg| message_to_transcript_entry(session_id, msg))
            .collect();

        let stats = compute_stats(&transcript, &session);

        Ok(SessionExport {
            id: session.id,
            label: session.label,
            channel: session.channel,
            owner: session.owner,
            agent_id: session.agent_id,
            kind: session.kind,
            created_at: session.created_at,
            updated_at: session.updated_at,
            total_tokens: session.total_tokens,
            model_override: session.model_override,
            transcript,
            stats,
        })
    }

    /// Get session statistics (message count, token breakdown, tool call summary).
    pub fn session_stats(&self, session_id: &str) -> eyre::Result<SessionStats> {
        let session = self
            .get(session_id)?
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;

        let transcript: Vec<TranscriptEntry> = session
            .messages
            .iter()
            .map(|msg| message_to_transcript_entry(session_id, msg))
            .collect();

        Ok(compute_stats(&transcript, &session))
    }
}

/// Convert a `SessionMessage` into a `TranscriptEntry`.
pub(crate) fn message_to_transcript_entry(
    session_id: &str,
    msg: &SessionMessage,
) -> TranscriptEntry {
    let tool_metadata = msg.tool_data.as_ref().and_then(|td| {
        Some(ToolMetadata {
            name: td.get("name")?.as_str()?.to_string(),
            input: td.get("input").cloned().unwrap_or(serde_json::Value::Null),
            output: td.get("output").cloned().unwrap_or(serde_json::Value::Null),
            duration_ms: td.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0),
            success: td.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
        })
    });

    // For assistant messages, derive token usage from the stored token count.
    // The session stores total tokens per message; for assistant messages this
    // is input_tokens + output_tokens combined. We record it as output_tokens
    // since that's the dominant cost for assistant turns.
    let token_usage = if msg.role == MessageRole::Assistant && msg.tokens > 0 {
        Some(TurnTokenUsage {
            input_tokens: 0,
            output_tokens: msg.tokens,
        })
    } else {
        None
    };

    TranscriptEntry {
        id: msg.id.clone(),
        session_id: session_id.to_string(),
        timestamp: msg.timestamp,
        role: msg.role.clone(),
        content: msg.content.clone(),
        tool_metadata,
        token_usage,
        desk: msg.desk.clone(),
    }
}

/// Paginate transcript entries: return `limit` entries before `before_id` (reverse-chronological).
pub(crate) fn paginate_entries(
    entries: Vec<TranscriptEntry>,
    before_id: Option<&str>,
    limit: usize,
) -> eyre::Result<ArchivePage> {
    let end = match before_id {
        Some(id) => entries
            .iter()
            .position(|e| e.id == id)
            .unwrap_or(entries.len()),
        None => entries.len(),
    };
    let start = end.saturating_sub(limit);
    let has_more = start > 0;
    let page = entries[start..end].to_vec();
    Ok(ArchivePage {
        messages: page,
        has_more,
    })
}

/// Compute aggregated statistics from transcript entries.
pub(crate) fn compute_stats(transcript: &[TranscriptEntry], session: &Session) -> SessionStats {
    let mut messages_by_role: HashMap<String, usize> = HashMap::new();
    let mut tokens_by_role: HashMap<String, u64> = HashMap::new();
    let mut total_input_tokens = 0u64;
    let mut total_output_tokens = 0u64;

    let mut tool_total = 0usize;
    let mut tool_succeeded = 0usize;
    let mut tool_failed = 0usize;
    let mut tool_total_duration = 0u64;
    let mut by_tool: HashMap<String, ToolStats> = HashMap::new();

    for entry in transcript {
        let role_str = serde_json::to_string(&entry.role).unwrap_or_default();
        // serde serialises as quoted string, strip quotes
        let role_key = role_str.trim_matches('"').to_string();

        *messages_by_role.entry(role_key.clone()).or_insert(0) += 1;

        if let Some(ref usage) = entry.token_usage {
            *tokens_by_role.entry(role_key).or_insert(0) +=
                (usage.input_tokens + usage.output_tokens) as u64;
            total_input_tokens += usage.input_tokens as u64;
            total_output_tokens += usage.output_tokens as u64;
        }

        if let Some(ref tool_meta) = entry.tool_metadata {
            tool_total += 1;
            if tool_meta.success {
                tool_succeeded += 1;
            } else {
                tool_failed += 1;
            }
            tool_total_duration += tool_meta.duration_ms;

            let stats = by_tool.entry(tool_meta.name.clone()).or_insert(ToolStats {
                count: 0,
                succeeded: 0,
                failed: 0,
                total_duration_ms: 0,
            });
            stats.count += 1;
            if tool_meta.success {
                stats.succeeded += 1;
            } else {
                stats.failed += 1;
            }
            stats.total_duration_ms += tool_meta.duration_ms;
        }
    }

    let duration_seconds = if transcript.len() >= 2 {
        let first = transcript.first().map(|e| e.timestamp);
        let last = transcript.last().map(|e| e.timestamp);
        first.zip(last).map(|(f, l)| (l - f).num_seconds())
    } else {
        None
    };

    SessionStats {
        message_count: transcript.len(),
        messages_by_role,
        tokens_by_role,
        total_tokens: session.total_tokens,
        total_input_tokens,
        total_output_tokens,
        tool_calls: ToolCallSummary {
            total: tool_total,
            succeeded: tool_succeeded,
            failed: tool_failed,
            total_duration_ms: tool_total_duration,
            by_tool,
        },
        duration_seconds,
    }
}

/// Split a search query into distinct lowercase terms. Whitespace is the only
/// delimiter so ticket ids and URLs (`DW-546`, `https://…`) stay single terms.
pub(crate) fn search_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .fold(Vec::new(), |mut terms, term| {
            if !terms.contains(&term) {
                terms.push(term);
            }
            terms
        })
}

/// Count of distinct `terms` occurring as substrings of `content_lower` (already
/// lowercased). A message covering more query terms ranks above one covering
/// fewer, so a multi-word query surfaces its best matches first instead of
/// demanding the whole phrase appear verbatim.
pub(crate) fn coordination_score(content_lower: &str, terms: &[String]) -> usize {
    terms
        .iter()
        .filter(|t| content_lower.contains(t.as_str()))
        .count()
}

/// Rank scored matches by coverage then recency and keep the top `limit`.
pub(crate) fn rank_search_results(
    mut scored: Vec<(usize, SessionSearchResult)>,
    limit: usize,
) -> Vec<SessionSearchResult> {
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.timestamp.cmp(&a.1.timestamp))
    });
    scored.into_iter().take(limit).map(|(_, r)| r).collect()
}

/// Build a search-result excerpt: the matched turn (marked with `»`) plus the
/// turns immediately around it, so the agent gets the surrounding exchange when
/// drilling into past detail — not just a snippet of one message. Each turn is
/// capped to keep results bounded.
fn format_search_excerpt(messages: &[SessionMessage], hit: usize) -> String {
    const RADIUS: usize = 2;
    let start = hit.saturating_sub(RADIUS);
    let end = (hit + RADIUS + 1).min(messages.len());
    format_excerpt_rows(messages[start..end].iter().enumerate().map(|(offset, m)| {
        (
            start + offset == hit,
            format!("{:?}", m.role).to_lowercase(),
            m.content.clone(),
        )
    }))
}

/// Render a search excerpt from `(is_hit, role, content)` rows already
/// gathered around the match (the hit marked with `»`). Shared by the file
/// store (slicing live `SessionMessage`s) and the Postgres store (selecting
/// archive rows by seq) so excerpt formatting lives in exactly one place.
pub(crate) fn format_excerpt_rows(rows: impl Iterator<Item = (bool, String, String)>) -> String {
    const PER_MSG_CHARS: usize = 600;
    rows.map(|(is_hit, role, content)| {
        let marker = if is_hit { "» " } else { "  " };
        let content = if content.chars().count() > PER_MSG_CHARS {
            let head: String = content.chars().take(PER_MSG_CHARS).collect();
            format!("{head}…")
        } else {
            content
        };
        format!("{marker}[{role}] {content}")
    })
    .collect::<Vec<_>>()
    .join("\n")
}

/// Compute context-window usage for an already-loaded session. Shared by
/// every store's `get_context_usage` so the utilisation/threshold logic
/// isn't copied per backend.
pub(crate) fn context_usage_for(session: &Session) -> SessionContextUsage {
    let tokens_used = session.estimated_total_tokens();
    let tokens_max = DISPLAY_CONTEXT_TOKENS_FALLBACK;
    let utilization = tokens_used as f32 / tokens_max as f32;
    SessionContextUsage {
        tokens_used,
        tokens_max,
        utilization,
        needs_compaction: utilization >= 0.8,
        model_override: session.model_override.clone(),
        message_count: session.messages.len(),
    }
}

/// Extract the tool-call records from an already-loaded session. Shared by
/// every store's `get_session_tool_calls`.
pub(crate) fn tool_calls_for(session: &Session) -> Vec<ToolCallRecord> {
    session
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .filter_map(|m| {
            let td = m.tool_data.as_ref()?;
            Some(ToolCallRecord {
                message_id: m.id.clone(),
                tool_name: td.get("name")?.as_str()?.to_string(),
                input: td.get("input").cloned().unwrap_or(serde_json::Value::Null),
                output: td.get("output").cloned().unwrap_or(serde_json::Value::Null),
                success: td.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
                duration_ms: td.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0),
                timestamp: m.timestamp,
            })
        })
        .collect()
}

/// Write bytes to a file atomically via a temp file + rename.
fn atomic_write(path: &Path, data: &[u8]) -> eyre::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| eyre::eyre!("No parent directory for {}", path.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| eyre::eyre!("Failed to create temp file in {}: {e}", dir.display()))?;
    tmp.write_all(data)
        .map_err(|e| eyre::eyre!("Failed to write temp file: {e}"))?;
    tmp.flush()
        .map_err(|e| eyre::eyre!("Failed to flush temp file: {e}"))?;
    tmp.persist(path)
        .map_err(|e| eyre::eyre!("Failed to persist temp file to {}: {e}", path.display()))?;
    Ok(())
}

/// What every store's turn journal promises, run against each backend.
#[cfg(test)]
pub(crate) fn assert_reassign_owner_contract(store: &dyn SessionStore) {
    let agent = "agent-a";
    let mut kept = store.get_or_create("c1", "alice", agent).unwrap();
    kept.add_user_message("hello", None);
    store.save(&kept).unwrap();
    let clashing = store.get_or_create("c2", "alice", agent).unwrap();
    let bobs = store.get_or_create("c2", "bob", agent).unwrap();
    let carol = store.get_or_create("c3", "carol", agent).unwrap();

    let mut moved = store.reassign_owner("alice", "bob", "tag").unwrap();
    moved.sort_by(|a, b| a.channel.cmp(&b.channel));
    assert_eq!(
        moved,
        vec![
            Reassigned {
                session_id: kept.id.clone(),
                channel: "c1".into(),
                renamed: false,
            },
            Reassigned {
                session_id: clashing.id.clone(),
                channel: reassigned_channel("c2", "tag"),
                renamed: true,
            },
        ]
    );
    assert!(store.list_for_owner("alice", 10).unwrap().is_empty());
    assert_eq!(store.find("c2", "bob").unwrap().unwrap().id, bobs.id);
    assert_eq!(
        store.find("c2#from-tag", "bob").unwrap().unwrap().id,
        clashing.id
    );
    assert_eq!(store.find("c1", "bob").unwrap().unwrap().id, kept.id);
    let continued = store.get(&kept.id).unwrap().unwrap();
    assert_eq!(continued.owner, "bob");
    assert_eq!(continued.messages.len(), 1, "the history moves with it");
    assert_eq!(store.list_for_owner("carol", 10).unwrap()[0].id, carol.id);
    assert!(store
        .reassign_owner("alice", "bob", "tag")
        .unwrap()
        .is_empty());
}

/// Every owner is counted once per session it holds, sealed ones included.
#[cfg(test)]
pub(crate) fn assert_owners_contract(store: &dyn SessionStore) {
    let agent = "agent-a";
    let first = store.get_or_create("c1", "alice", agent).unwrap();
    store.start_fresh("c1", "alice", agent).unwrap();
    store.get_or_create("c2", "alice", agent).unwrap();
    store.get_or_create("c1", "shared:1700.1", agent).unwrap();
    assert!(store.get(&first.id).unwrap().unwrap().is_sealed());

    assert_eq!(
        store.owners().unwrap(),
        BTreeMap::from([("alice".to_string(), 3), ("shared:1700.1".to_string(), 1)])
    );
}

#[cfg(test)]
pub(crate) fn assert_turn_journal_contract(store: &dyn SessionStore, session_id: &str) {
    let finished = |call: &str| JournalEntry {
        call: call.into(),
        at: Utc::now(),
        step: JournalStep::Finished {
            message: SessionMessage {
                id: call.into(),
                ..SessionMessage::tool(
                    "shell",
                    &serde_json::json!({}),
                    &serde_json::json!("ok"),
                    true,
                    5,
                    None,
                )
            },
        },
    };
    let pending = JournalEntry {
        call: "c2".into(),
        at: Utc::now(),
        step: JournalStep::Pending {
            tool: "coding.session".into(),
            input: serde_json::json!({"message": "fix it"}),
            plan: serde_json::json!({"round": "r1"}),
            actor: None,
        },
    };
    assert!(store.journal(session_id).unwrap().is_empty());
    let checkpoint = |mark: &str| JournalEntry {
        call: "c2".into(),
        at: Utc::now(),
        step: JournalStep::Checkpoint { mark: mark.into() },
    };
    store.journal_append(session_id, &finished("c1")).unwrap();
    store.journal_append(session_id, &pending).unwrap();
    store
        .journal_append(session_id, &checkpoint("r1.0.0"))
        .unwrap();
    store
        .journal_append(session_id, &checkpoint("r1.0.1"))
        .unwrap();
    store
        .journal_append(session_id, &checkpoint("r1.0.0"))
        .unwrap();
    store.journal_append(session_id, &finished("c1")).unwrap();
    store.journal_append(session_id, &finished("c2")).unwrap();
    let kept: Vec<(String, String)> = store
        .journal(session_id)
        .unwrap()
        .iter()
        .map(|entry| (entry.call.clone(), entry.key()))
        .collect();
    assert_eq!(
        kept,
        vec![
            ("c1".to_string(), "finished".to_string()),
            ("c2".to_string(), "pending".to_string()),
            ("c2".to_string(), "checkpoint:r1.0.0".to_string()),
            ("c2".to_string(), "checkpoint:r1.0.1".to_string()),
            ("c2".to_string(), "finished".to_string()),
        ],
        "in order, each entry once"
    );
    let JournalStep::Pending { plan, .. } = &store.journal(session_id).unwrap()[1].step else {
        panic!("the second entry is the pending one");
    };
    assert_eq!(plan, &serde_json::json!({"round": "r1"}));
    assert!(store
        .journal("another-session")
        .unwrap_or_default()
        .is_empty());
    let listed = |prefix: &str| {
        store
            .journals(prefix)
            .unwrap()
            .contains(&session_id.to_string())
    };
    assert!(listed("") && listed(&session_id[..4]));
    assert!(!listed("fanout-"));
    store.journal_clear(session_id).unwrap();
    assert!(store.journal(session_id).unwrap().is_empty());
    assert!(!listed(""));
    store.journal_clear(session_id).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_in_memory_store_reassigns_owners_keeping_both_on_a_shared_channel() {
        assert_reassign_owner_contract(&InMemorySessionStore::new());
    }

    #[test]
    fn the_file_store_reassigns_owners_keeping_both_on_a_shared_channel() {
        let dir = tempfile::tempdir().unwrap();
        assert_reassign_owner_contract(&JsonlSessionStore::new(dir.path()).unwrap());
    }

    #[test]
    fn the_in_memory_store_counts_sessions_per_owner() {
        assert_owners_contract(&InMemorySessionStore::new());
    }

    #[test]
    fn the_file_store_counts_sessions_per_owner_after_reopening() {
        let dir = tempfile::tempdir().unwrap();
        assert_owners_contract(&JsonlSessionStore::new(dir.path()).unwrap());
        let reopened = JsonlSessionStore::new(dir.path()).unwrap();
        assert_eq!(reopened.owners().unwrap().get("alice"), Some(&3));
    }

    #[test]
    fn the_in_memory_turn_journal_keeps_its_contract() {
        let store = InMemorySessionStore::new();
        let session = store.get_or_create("c", "o", "a").unwrap();
        assert_turn_journal_contract(&store, &session.id);
    }

    #[cfg(unix)]
    fn other_bits(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o077
    }

    #[cfg(unix)]
    #[test]
    fn opening_the_file_store_closes_a_group_readable_sessions_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o2775)).unwrap();

        JsonlSessionStore::new(dir.path()).expect("store");

        assert_eq!(other_bits(&sessions), 0);
    }

    #[cfg(unix)]
    #[test]
    fn every_file_the_store_appends_to_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path()).expect("store");
        let mut session = store.get_or_create("c", "o", "a").unwrap();
        let id = session.add_user_message("hello", None);
        store.save(&session).unwrap();
        let message = session.messages.iter().find(|m| m.id == id).unwrap();
        store.append_to_archive(&session.id, message).unwrap();
        store
            .journal_append(
                &session.id,
                &JournalEntry {
                    call: "c1".into(),
                    at: Utc::now(),
                    step: JournalStep::Pending {
                        tool: "t".into(),
                        input: serde_json::json!({}),
                        plan: serde_json::json!({}),
                        actor: None,
                    },
                },
            )
            .unwrap();

        for path in [
            store.messages_path(&session.id),
            store.meta_path(&session.id),
            store.archive_path(&session.id),
            store.journal_path(&session.id),
        ] {
            assert_eq!(
                other_bits(&path),
                0,
                "{} is readable beyond the hub",
                path.display()
            );
        }
    }

    #[test]
    fn the_file_turn_journal_keeps_its_contract_and_survives_a_torn_line() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path()).expect("store");
        let session = store.get_or_create("c", "o", "a").unwrap();
        assert_turn_journal_contract(&store, &session.id);

        let entry = JournalEntry {
            call: "c1".into(),
            at: Utc::now(),
            step: JournalStep::Pending {
                tool: "t".into(),
                input: serde_json::json!({}),
                plan: serde_json::json!({}),
                actor: None,
            },
        };
        store.journal_append(&session.id, &entry).unwrap();
        let path = store.journal_path(&session.id);
        let mut torn = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        torn.write_all(br#"{"call":"c2","at":"#).unwrap();
        let reopened = JsonlSessionStore::new(dir.path()).expect("reopened store");
        assert_eq!(reopened.journal(&session.id).unwrap().len(), 1);
        reopened
            .journal_append(
                &session.id,
                &JournalEntry {
                    call: "c3".into(),
                    ..entry.clone()
                },
            )
            .unwrap();
        let calls: Vec<String> = reopened
            .journal(&session.id)
            .unwrap()
            .into_iter()
            .map(|entry| entry.call)
            .collect();
        assert_eq!(calls, vec!["c1", "c3"], "a torn line costs only itself");

        reopened.delete(&session.id).unwrap();
        assert!(
            !path.exists(),
            "a deleted session takes its journal with it"
        );
    }

    #[test]
    fn install_state_is_the_turn_record_and_the_journal() {
        assert!(is_install_state("s1.turn.json"));
        assert!(is_install_state("s1.journal.jsonl"));
        assert!(!is_install_state("s1.jsonl"));
        assert!(!is_install_state("s1.meta.json"));
    }

    #[test]
    fn successor_carries_only_thread_identity() {
        let mut current = Session::new("mcp:u1", "u1", "allan");
        current.label = Some("mcp:u1:u1".into());
        current.set_seq(3);
        current.model_override = Some("fast".into());
        current.add_user_message("hello", None);

        let next = current.successor();
        assert_ne!(next.id, current.id);
        assert_eq!(next.channel, "mcp:u1");
        assert_eq!(next.owner, "u1");
        assert_eq!(next.agent_id, "allan");
        assert_eq!(next.label.as_deref(), Some("mcp:u1:u1"));
        assert_eq!(next.seq(), 4);
        assert_eq!(next.prev_session_id(), None);
        assert!(next.messages.is_empty());
        assert_eq!(next.model_override, None);
        assert!(!next.is_sealed());
    }

    /// `reopen` models a process restart between resets: a file store must
    /// then seal through the shells its fresh cache holds. `None` keeps the
    /// same store.
    fn assert_start_fresh_restarts_the_conversation(
        store: &dyn SessionStore,
        reopen: impl FnOnce() -> Option<Box<dyn SessionStore>>,
    ) {
        let (channel, owner, agent) = ("mcp:u1", "u1", "allan");
        let first = store.start_fresh(channel, owner, agent).unwrap();
        assert_eq!(
            first.seq(),
            0,
            "no active session: the first one is created"
        );
        assert_eq!(first.label.as_deref(), Some("mcp:u1:u1"));

        let mut active = store.get_or_create(channel, owner, agent).unwrap();
        assert_eq!(active.id, first.id);
        active.add_user_message("hello", None);
        store.save(&active).unwrap();
        let second = store.start_fresh(channel, owner, agent).unwrap();

        let reopened = reopen();
        let store = reopened.as_deref().unwrap_or(store);
        let third = store.start_fresh(channel, owner, agent).unwrap();
        assert_eq!((second.seq(), third.seq()), (1, 2));
        assert!(
            second.prev_session_id().is_none(),
            "a reset carries nothing over"
        );
        assert!(third.prev_session_id().is_none());

        let live = store.find(channel, owner).unwrap().unwrap();
        assert_eq!(live.id, third.id, "only the newest successor is active");
        assert!(live.messages.is_empty());
        let sealed_first = store.get(&first.id).unwrap().unwrap();
        assert!(sealed_first.is_sealed());
        assert_eq!(
            sealed_first.messages.len(),
            1,
            "sealing keeps the predecessor's transcript"
        );
        assert!(store.get(&second.id).unwrap().unwrap().is_sealed());
    }

    #[test]
    fn in_memory_start_fresh_restarts_the_conversation() {
        assert_start_fresh_restarts_the_conversation(&InMemorySessionStore::new(), || None);
    }

    #[test]
    fn jsonl_start_fresh_restarts_the_conversation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("store");
        assert_start_fresh_restarts_the_conversation(&store, || {
            Some(Box::new(
                JsonlSessionStore::new(dir.path()).expect("reopened store"),
            ))
        });
    }

    #[test]
    fn from_bytes_keeps_text_readable_and_base64s_the_rest() {
        use base64::Engine as _;
        let yaml = b"---\nviolations:\n  - id: 1\n";

        // Text-like types must arrive readable — a consumer inlines `data`
        // verbatim, so base64 here reaches the model as gibberish.
        for mime in ["text/plain", "text/yaml", "application/json", "text/csv"] {
            let att = SessionAttachment::from_bytes("f.yml", mime, yaml);
            assert_eq!(
                att.data,
                String::from_utf8_lossy(yaml),
                "{mime} must decode to text"
            );
        }

        // Everything else stays base64 for the binary pipelines to decode.
        for mime in ["image/png", "application/pdf", "application/octet-stream"] {
            let att = SessionAttachment::from_bytes("f.bin", mime, yaml);
            assert_eq!(
                att.data,
                base64::engine::general_purpose::STANDARD.encode(yaml),
                "{mime} must stay base64"
            );
        }
    }

    #[test]
    fn from_bytes_survives_invalid_utf8_in_text() {
        let att = SessionAttachment::from_bytes("f.txt", "text/plain", &[0xff, 0xfe, b'h', b'i']);
        assert!(att.data.contains("hi"), "lossy decode, never a panic");
    }

    fn aged_session(channel: &str, agent: &str, age: chrono::Duration) -> Session {
        let mut s = Session::new(channel, "owner", agent);
        s.updated_at = Utc::now() - age;
        s
    }

    #[test]
    fn active_mcp_env_set_get_clear_round_trip() {
        let mut s = Session::new("slack:ws:C1:1", "owner", "agent");
        assert!(s.active_mcp_envs().is_empty());

        s.set_active_mcp_env("harbor", "staging");
        s.set_active_mcp_env("xero", "demo");
        let envs = s.active_mcp_envs();
        assert_eq!(envs.get("harbor").map(String::as_str), Some("staging"));
        assert_eq!(envs.get("xero").map(String::as_str), Some("demo"));

        // Re-pointing one workspace overwrites only that key.
        s.set_active_mcp_env("harbor", "prod");
        assert_eq!(
            s.active_mcp_envs().get("harbor").map(String::as_str),
            Some("prod")
        );

        // Clearing reverts that workspace to default (drops the key) and leaves
        // the other override intact.
        s.clear_active_mcp_env("harbor");
        let envs = s.active_mcp_envs();
        assert!(!envs.contains_key("harbor"));
        assert_eq!(envs.get("xero").map(String::as_str), Some("demo"));
    }

    #[test]
    fn active_mcp_envs_survive_metadata_serialization() {
        // The override lives in `metadata`, which is persisted — so it must
        // round-trip through serde the way the stores save/load a session.
        let mut s = Session::new("slack:ws:C1:1", "owner", "agent");
        s.set_active_mcp_env("harbor", "staging");
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.active_mcp_envs().get("harbor").map(String::as_str),
            Some("staging")
        );
    }

    #[test]
    fn get_or_create_round_trip_preserves_seed() {
        // Linchpin for session-context seeding: a pre-seeded fresh session must be
        // the same one the runner's own `get_or_create(channel, owner, agent)`
        // returns — same id, seed intact — not a new empty session.
        let store = InMemorySessionStore::new();
        let (key, owner, agent) = ("slack:ws:C1:100.1", "U1", "atlas");

        let mut session = store.get_or_create(key, owner, agent).unwrap();
        assert!(session.messages.is_empty());
        let first_id = session.id.clone();
        session.add_system_message("## Recent messages in this channel\nAlice: hi");
        store.save(&session).unwrap();

        let reloaded = store.get_or_create(key, owner, agent).unwrap();
        assert_eq!(reloaded.id, first_id);
        assert_eq!(reloaded.messages.len(), 1);
        assert!(reloaded.messages[0].content.contains("Recent messages"));
    }

    #[test]
    fn jsonl_get_or_create_round_trip_preserves_seed() {
        // Same linchpin against the file store the hub actually runs: shells and
        // lazy hydration could behave differently from the in-memory clone.
        let dir = tempfile::tempdir().expect("temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("store");
        let (key, owner, agent) = ("slack:ws:C1:100.1", "U1", "atlas");

        let mut session = store.get_or_create(key, owner, agent).unwrap();
        assert!(session.messages.is_empty());
        let first_id = session.id.clone();
        session.add_system_message("## Recent messages in this channel\nAlice: hi");
        store.save(&session).unwrap();

        let reloaded = store.get_or_create(key, owner, agent).unwrap();
        assert_eq!(reloaded.id, first_id);
        assert_eq!(reloaded.messages.len(), 1);
        assert!(reloaded.messages[0].content.contains("Recent messages"));
    }

    #[test]
    fn playbook_progress_round_trips_and_clears() {
        use crate::playbook_state::{ArtifactRef, PlaybookProgress};

        let mut s = Session::new("slack:c", "owner", "agent");
        assert!(s.playbook().is_none());

        let mut progress =
            PlaybookProgress::new("design-impl-review", "design", "n1", "playbooks/s");
        progress.step_id = "review".into();
        progress.artifacts.push(ArtifactRef {
            step_id: "design".into(),
            path: "p/design.md".into(),
            synopsis: "syn".into(),
        });
        s.set_playbook(&progress);
        assert_eq!(s.playbook().as_ref(), Some(&progress));

        // Survives a serialize/deserialize of the whole session (as .meta.json does).
        let json = serde_json::to_string(&s).unwrap();
        let restored: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.playbook().as_ref(), Some(&progress));

        s.clear_playbook();
        assert!(s.playbook().is_none());
    }

    #[test]
    fn replay_prefix_round_trips_through_metadata() {
        let mut s = Session::new("slack:c", "owner", "agent");
        assert!(s.replay_prefix().is_none());
        let updated_at = s.updated_at;

        let prefix = serde_json::json!({"len": 42, "digest": 7});
        s.set_replay_prefix(prefix.clone());
        assert_eq!(s.replay_prefix(), Some(&prefix));
        assert_eq!(s.updated_at, updated_at, "a fingerprint is not activity");

        let json = serde_json::to_string(&s).unwrap();
        let restored: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.replay_prefix(), Some(&prefix));
    }

    #[test]
    fn system_prefix_round_trips_through_metadata() {
        let mut s = Session::new("slack:c", "owner", "agent");
        assert!(s.system_prefix().is_none());
        let updated_at = s.updated_at;

        let prefix = serde_json::json!({"len": 9000, "digest": 11});
        s.set_system_prefix(prefix.clone());
        assert_eq!(s.system_prefix(), Some(&prefix));
        assert_eq!(s.updated_at, updated_at, "a fingerprint is not activity");

        let json = serde_json::to_string(&s).unwrap();
        let restored: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.system_prefix(), Some(&prefix));
    }

    #[test]
    fn list_since_filters_by_agent_and_cutoff() {
        let store = InMemorySessionStore::new();
        let cutoff = Utc::now() - chrono::Duration::hours(1);
        let fresh = chrono::Duration::minutes(30);
        let stale = chrono::Duration::hours(2);

        store
            .save(&aged_session("slack:fresh-allan", "allan", fresh))
            .unwrap();
        store
            .save(&aged_session("slack:stale-allan", "allan", stale))
            .unwrap();
        store
            .save(&aged_session("slack:fresh-brian", "brian", fresh))
            .unwrap();

        let result = store.list_since("allan", cutoff, 100).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].channel, "slack:fresh-allan");
    }

    #[test]
    fn list_since_returns_results_newest_first() {
        let store = InMemorySessionStore::new();
        let cutoff = Utc::now() - chrono::Duration::hours(1);

        store
            .save(&aged_session(
                "older",
                "allan",
                chrono::Duration::minutes(45),
            ))
            .unwrap();
        store
            .save(&aged_session(
                "newer",
                "allan",
                chrono::Duration::minutes(5),
            ))
            .unwrap();

        let result = store.list_since("allan", cutoff, 100).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].channel, "newer");
        assert_eq!(result[1].channel, "older");
    }

    #[test]
    fn list_since_respects_limit() {
        let store = InMemorySessionStore::new();
        let cutoff = Utc::now() - chrono::Duration::hours(1);
        for i in 0..5 {
            store
                .save(&aged_session(
                    &format!("ch-{i}"),
                    "allan",
                    chrono::Duration::minutes(i * 5),
                ))
                .unwrap();
        }
        let result = store.list_since("allan", cutoff, 2).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn list_since_cutoff_is_inclusive() {
        let store = InMemorySessionStore::new();
        let mut s = Session::new("slack:exact", "owner", "allan");
        let pinned = Utc::now() - chrono::Duration::hours(1);
        s.updated_at = pinned;
        store.save(&s).unwrap();

        // cutoff == session.updated_at — must include (retry semantics).
        let result = store.list_since("allan", pinned, 100).unwrap();
        assert_eq!(result.len(), 1);
    }

    fn owned_session(channel: &str, owner: &str, age: chrono::Duration) -> Session {
        let mut s = Session::new(channel, owner, "allan");
        s.updated_at = Utc::now() - age;
        s
    }

    #[test]
    fn jsonl_list_for_owner_filters_and_sorts_newest_first() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("store");

        store
            .save(&owned_session(
                "slack:D1",
                "alice",
                chrono::Duration::minutes(45),
            ))
            .unwrap();
        store
            .save(&owned_session(
                "slack:D2",
                "alice",
                chrono::Duration::minutes(5),
            ))
            .unwrap();
        store
            .save(&owned_session(
                "slack:D3",
                "bob",
                chrono::Duration::minutes(1),
            ))
            .unwrap();

        let result = store.list_for_owner("alice", 100).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].channel, "slack:D2");
        assert_eq!(result[1].channel, "slack:D1");
    }

    #[test]
    fn jsonl_list_for_owner_respects_limit_and_ignores_other_owners() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("store");

        for i in 0..5 {
            store
                .save(&owned_session(
                    &format!("ch-{i}"),
                    "alice",
                    chrono::Duration::minutes(i * 5),
                ))
                .unwrap();
        }
        store
            .save(&owned_session("ch-bob", "bob", chrono::Duration::zero()))
            .unwrap();

        let result = store.list_for_owner("alice", 2).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|s| s.owner == "alice"));
        // Newest two survive the limit.
        assert_eq!(result[0].channel, "ch-0");
        assert_eq!(result[1].channel, "ch-1");

        assert!(store.list_for_owner("mallory", 10).unwrap().is_empty());
    }

    #[test]
    fn claude_chat_metadata_roundtrip() {
        let mut session = Session::new("slack:ws:C123:ts", "U_USER", "agent");

        assert!(!session.is_claude_chat());
        assert!(session.claude_session_uuid().is_none());
        assert!(session.claude_chat_repo().is_none());
        assert!(session.claude_chat_agent_id().is_none());

        session.mark_claude_chat("workforce", "U_USER", Some("allan"));
        assert!(session.is_claude_chat());
        assert_eq!(session.claude_chat_repo(), Some("workforce"));
        assert_eq!(session.claude_chat_agent_id(), Some("allan"));

        assert!(session.claude_session_uuid().is_none());
        let uuid1 = session.get_or_init_claude_uuid();
        assert!(Uuid::parse_str(&uuid1).is_ok());
        assert_eq!(session.claude_session_uuid(), Some(uuid1.as_str()));

        let uuid2 = session.get_or_init_claude_uuid();
        assert_eq!(uuid1, uuid2);
    }

    #[test]
    fn clear_claude_session_uuid_lets_next_init_mint_a_fresh_id() {
        let mut session = Session::new("slack:ws:C123:ts", "U_USER", "agent");
        let original = session.get_or_init_claude_uuid();
        session.clear_claude_session_uuid();
        assert!(session.claude_session_uuid().is_none());
        let regenerated = session.get_or_init_claude_uuid();
        assert_ne!(original, regenerated, "should mint a new UUID after clear");
    }

    #[test]
    fn driver_claude_uuid_is_independent_of_cc_uuid() {
        // The driver slot (used by `coding.session`) must not collide with the
        // `/cc` slot when both share a thread's session — operating each must
        // not disturb the other.
        let mut session = Session::new("slack:ws:C123:ts", "U_USER", "agent");
        assert!(session.driver_claude_session_uuid("proj").is_none());

        let driver = session.get_or_init_driver_claude_uuid("proj");
        assert!(Uuid::parse_str(&driver).is_ok());
        assert_eq!(
            session.driver_claude_session_uuid("proj"),
            Some(driver.as_str())
        );
        assert_eq!(
            session.get_or_init_driver_claude_uuid("proj"),
            driver,
            "second init returns the same UUID"
        );

        let cc = session.get_or_init_claude_uuid();
        assert_ne!(driver, cc, "driver and /cc UUIDs must be distinct");

        // Clearing the driver slot leaves the /cc slot intact.
        session.clear_driver_claude_session_uuid("proj");
        assert!(session.driver_claude_session_uuid("proj").is_none());
        assert_eq!(
            session.claude_session_uuid(),
            Some(cc.as_str()),
            "/cc UUID must survive a driver-slot clear"
        );
        assert_ne!(
            session.get_or_init_driver_claude_uuid("proj"),
            driver,
            "fresh driver UUID after clear"
        );
    }

    /// Each project keeps its own driver UUID + rotation counters: in one
    /// Slack session, alternating `coding.session` calls between projects
    /// must resume each project's own Claude Code transcript instead of
    /// treating every switch as a missing worktree and cold-starting.
    #[test]
    fn driver_claude_uuid_is_keyed_per_project() {
        let mut session = Session::new("slack:ws:C123:ts", "U_USER", "agent");

        let a = session.get_or_init_driver_claude_uuid("alpha");
        let b = session.get_or_init_driver_claude_uuid("beta");
        assert_ne!(a, b, "projects must not share a driver UUID");
        assert_eq!(session.get_or_init_driver_claude_uuid("alpha"), a);
        assert_eq!(session.get_or_init_driver_claude_uuid("beta"), b);

        session.record_driver_chat_turn(Utc::now(), "alpha");
        assert_eq!(session.driver_chat_turn_count("alpha"), 1);
        assert_eq!(
            session.driver_chat_turn_count("beta"),
            0,
            "rotation counters must not bleed across projects"
        );

        // Clearing one project leaves the other intact.
        session.clear_driver_claude_session_uuid("alpha");
        assert!(session.driver_claude_session_uuid("alpha").is_none());
        assert_eq!(session.driver_claude_session_uuid("beta"), Some(b.as_str()));
    }

    /// Legacy un-keyed driver slots (pre-project-keying) are dropped on the
    /// first keyed init rather than adopted — adopting would re-introduce
    /// the cross-project ambiguity, and the stale keys must not linger.
    #[test]
    fn driver_claude_uuid_drops_legacy_unkeyed_slot() {
        let mut session = Session::new("slack:ws:C123:ts", "U_USER", "agent");
        session.metadata_object_mut().insert(
            "driver_claude_session_uuid".to_string(),
            serde_json::Value::String("11111111-1111-1111-1111-111111111111".into()),
        );

        let minted = session.get_or_init_driver_claude_uuid("proj");
        assert_ne!(
            minted, "11111111-1111-1111-1111-111111111111",
            "legacy slot must not be adopted"
        );
        assert!(
            session.metadata.get("driver_claude_session_uuid").is_none(),
            "legacy slot must be removed"
        );
    }

    #[test]
    fn claude_chat_turn_count_starts_at_zero_and_bumps() {
        let mut session = Session::new("slack:ws:C123:ts", "U_USER", "agent");
        assert_eq!(session.claude_chat_turn_count(), 0);
        assert!(session.claude_chat_last_reply_at().is_none());

        let t0 = Utc::now();
        session.record_claude_chat_turn(t0);
        assert_eq!(session.claude_chat_turn_count(), 1);
        assert_eq!(
            session.claude_chat_last_reply_at().map(|t| t.to_rfc3339()),
            Some(t0.to_rfc3339())
        );

        let t1 = t0 + chrono::Duration::seconds(60);
        session.record_claude_chat_turn(t1);
        assert_eq!(session.claude_chat_turn_count(), 2);
        assert_eq!(
            session.claude_chat_last_reply_at().map(|t| t.to_rfc3339()),
            Some(t1.to_rfc3339())
        );
    }

    #[test]
    fn clear_claude_session_uuid_resets_rotation_counters() {
        let mut session = Session::new("slack:ws:C123:ts", "U_USER", "agent");
        let _ = session.get_or_init_claude_uuid();
        session.record_claude_chat_turn(Utc::now());
        assert_eq!(session.claude_chat_turn_count(), 1);
        assert!(session.claude_chat_last_reply_at().is_some());

        session.clear_claude_session_uuid();
        assert_eq!(session.claude_chat_turn_count(), 0);
        assert!(session.claude_chat_last_reply_at().is_none());
    }

    #[test]
    fn claude_chat_survives_serialization() {
        let mut session = Session::new("slack:ws:C123:ts", "U_USER", "agent");
        session.mark_claude_chat("workforce", "U_USER", Some("allan"));
        let uuid = session.get_or_init_claude_uuid();

        let json = serde_json::to_string(&session).unwrap();
        let round_tripped: Session = serde_json::from_str(&json).unwrap();

        assert!(round_tripped.is_claude_chat());
        assert_eq!(round_tripped.claude_chat_repo(), Some("workforce"));
        assert_eq!(round_tripped.claude_chat_agent_id(), Some("allan"));
        assert_eq!(round_tripped.claude_session_uuid(), Some(uuid.as_str()));
    }

    #[test]
    fn claude_chat_agent_id_absent_when_unpinned() {
        let mut session = Session::new("slack:ws:C123:ts", "U_USER", "agent");
        session.mark_claude_chat("workforce", "U_USER", None);
        assert!(session.is_claude_chat());
        assert_eq!(session.claude_chat_agent_id(), None);
    }

    #[test]
    fn a_chat_thread_remembers_which_coding_chat_opened_it() {
        let mut session = Session::new("slack:ws:C123:ts", "U_USER", "agent");
        session.mark_claude_chat("workforce", "U_USER", None);
        assert_eq!(
            session.claude_chat_engine(),
            None,
            "a thread before /cx is a /cc thread"
        );
        session.set_claude_chat_engine("cx");
        assert_eq!(session.claude_chat_engine(), Some("cx"));
        session.mark_claude_chat_project("harbor", "U_USER", None);
        assert_eq!(
            session.claude_chat_engine(),
            Some("cx"),
            "rebinding the repo keeps the engine"
        );
    }

    #[test]
    fn a_chat_threads_model_and_effort_outlive_a_rotation_and_a_rebind() {
        let mut session = Session::new("slack:ws:C123:ts", "U_USER", "agent");
        session.mark_claude_chat("workforce", "U_USER", None);
        assert_eq!(session.claude_chat_model(), None);
        session.set_claude_chat_choice(Some("astra"), Some("xhigh"));
        let _ = session.get_or_init_claude_uuid();
        session.clear_claude_session_uuid();
        session.mark_claude_chat_project("harbor", "U_USER", None);
        assert_eq!(session.claude_chat_model(), Some("astra"));
        assert_eq!(session.claude_chat_effort(), Some("xhigh"));
        session.set_claude_chat_choice(None, Some("low"));
        assert_eq!(session.claude_chat_model(), None);
        assert_eq!(session.claude_chat_effort(), Some("low"));
    }

    #[test]
    fn claude_chat_remark_clears_stale_agent_pin() {
        let mut session = Session::new("slack:ws:C123:ts", "U_USER", "agent");
        session.mark_claude_chat("workforce", "U_USER", Some("allan"));
        assert_eq!(session.claude_chat_agent_id(), Some("allan"));
        session.mark_claude_chat("workforce", "U_USER", None);
        assert_eq!(session.claude_chat_agent_id(), None);
    }

    #[test]
    fn mark_claude_chat_project_clears_repo() {
        let mut session = Session::new("slack:ws:C123:ts", "U_USER", "agent");
        session.mark_claude_chat("workforce", "U_USER", Some("allan"));
        session.mark_claude_chat_project("harbor", "U_USER", Some("allan"));
        assert_eq!(session.claude_chat_project(), Some("harbor"));
        assert!(session.claude_chat_repo().is_none());
    }

    #[test]
    fn mark_claude_chat_clears_project() {
        let mut session = Session::new("slack:ws:C123:ts", "U_USER", "agent");
        session.mark_claude_chat_project("harbor", "U_USER", Some("allan"));
        session.mark_claude_chat("workforce", "U_USER", Some("allan"));
        assert_eq!(session.claude_chat_repo(), Some("workforce"));
        assert!(session.claude_chat_project().is_none());
    }

    #[test]
    fn session_lifecycle() {
        let mut session = Session::new("slack", "kings", "allan");
        assert_eq!(session.message_count(), 0);

        session.add_system_message("You are Allan.");
        session.add_user_message("Hello!", None);
        session.add_assistant_message("Hi Kings!", 10);

        assert_eq!(session.message_count(), 3);
        assert!(session.total_tokens > 0);

        let recent = session.recent_messages(2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].role, MessageRole::User);
        assert_eq!(recent[1].role, MessageRole::Assistant);
    }

    #[test]
    fn context_window_truncation() {
        let mut session = Session::new("cli", "test", "agent");
        session.add_system_message("System prompt");

        // Add lots of messages
        for i in 0..100 {
            session.add_user_message(&format!("Message {}", i), None);
            session.add_assistant_message(&format!("Reply {}", i), 10);
        }

        // Get context with limited tokens — should include system + recent
        let context = session.context_messages(100);
        assert!(context.len() < session.message_count());
        assert_eq!(context[0].role, MessageRole::System);
    }

    #[test]
    fn in_memory_store() {
        let store = InMemorySessionStore::new();

        // Get or create
        let s1 = store.get_or_create("slack", "kings", "allan").unwrap();
        let s2 = store.get_or_create("slack", "kings", "allan").unwrap();
        assert_eq!(s1.id, s2.id); // Same session

        let s3 = store.get_or_create("clawdy", "kings", "allan").unwrap();
        assert_ne!(s1.id, s3.id); // Different channel, different session

        // Save and get
        let mut session = s1;
        session.add_user_message("test", None);
        store.save(&session).unwrap();

        let retrieved = store.get(&session.id).unwrap().unwrap();
        assert_eq!(retrieved.message_count(), 1);

        // List
        let all = store.list(None, 10).unwrap();
        assert_eq!(all.len(), 2);

        let slack_only = store.list(Some("slack"), 10).unwrap();
        assert_eq!(slack_only.len(), 1);

        // Delete
        store.delete(&session.id).unwrap();
        assert!(store.get(&session.id).unwrap().is_none());
    }

    // ─── JSONL Session Store Tests ───

    #[test]
    fn jsonl_store_create_and_retrieve() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");

        let session = store
            .get_or_create("slack", "kings", "allan")
            .expect("get_or_create failed");
        assert_eq!(session.channel, "slack");
        assert_eq!(session.owner, "kings");
        assert_eq!(session.agent_id, "allan");
        assert_eq!(session.label, Some("slack:kings".to_string()));

        // Same channel+owner returns same session
        let session2 = store
            .get_or_create("slack", "kings", "allan")
            .expect("get_or_create failed");
        assert_eq!(session.id, session2.id);

        // Different channel creates new session
        let session3 = store
            .get_or_create("cli", "kings", "allan")
            .expect("get_or_create failed");
        assert_ne!(session.id, session3.id);
    }

    #[test]
    fn jsonl_store_save_and_get() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");

        let mut session = store
            .get_or_create("slack", "kings", "allan")
            .expect("get_or_create failed");

        session.add_system_message("You are Allan.");
        session.add_user_message("Hello!", None);
        session.add_assistant_message("Hi there!", 15);
        store.save(&session).expect("save failed");

        let retrieved = store
            .get(&session.id)
            .expect("get failed")
            .expect("session not found");
        assert_eq!(retrieved.message_count(), 3);
        assert_eq!(retrieved.messages[0].role, MessageRole::System);
        assert_eq!(retrieved.messages[1].role, MessageRole::User);
        assert_eq!(retrieved.messages[2].role, MessageRole::Assistant);
        assert_eq!(retrieved.total_tokens, session.total_tokens);
    }

    const GH_SECRET: &str = concat!("ghp_AB", "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234");

    #[test]
    fn redact_session_masks_content_and_tool_data_and_is_idempotent() {
        let store = InMemorySessionStore::new();
        let mut session = store.get_or_create("slack", "kings", "allan").unwrap();
        session.add_user_message(&format!("my token is {GH_SECRET} ok"), None);
        session.add_assistant_message("done", 5);
        if let Some(last) = session.messages.last_mut() {
            last.tool_data = Some(serde_json::json!({
                "value": concat!("ghp_ZY", "XWVUTSRQPONMLKJIHGFEDCBA9876543210zzzz")
            }));
        }
        store.save(&session).unwrap();

        let stats = store.redact_secrets_in_session(&session.id, false).unwrap();
        assert_eq!(stats.messages_scanned, 2);
        assert_eq!(stats.messages_redacted, 2);

        let got = store.get(&session.id).unwrap().unwrap();
        let blob = serde_json::to_string(&got.messages).unwrap();
        assert!(!blob.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
        assert!(!blob.contains("ZYXWVUTSRQPONMLKJIHGFEDCBA"));

        // Second pass changes nothing — masked values don't re-match.
        let again = store.redact_secrets_in_session(&session.id, false).unwrap();
        assert_eq!(again.messages_redacted, 0);
    }

    #[test]
    fn redact_session_preserves_markers() {
        let store = InMemorySessionStore::new();
        let mut session = store.get_or_create("slack", "kings", "allan").unwrap();
        session.add_user_message(
            "use [[secret:datadog_api_key]] and \
             [[secret:0123456789abcdef0123456789abcdef type=GITHUB_PAT preview=ghp_...1234]]",
            None,
        );
        store.save(&session).unwrap();

        // Dry run and apply agree: markers are never counted as secrets.
        assert_eq!(
            store
                .redact_secrets_in_session(&session.id, true)
                .unwrap()
                .messages_redacted,
            0
        );
        let stats = store.redact_secrets_in_session(&session.id, false).unwrap();
        assert_eq!(stats.messages_redacted, 0);
        let got = store.get(&session.id).unwrap().unwrap();
        assert!(got.messages[0]
            .content
            .contains("[[secret:datadog_api_key]]"));
        assert!(got.messages[0].content.contains("type=GITHUB_PAT"));
    }

    #[test]
    fn redact_session_dry_run_reports_without_mutating() {
        let store = InMemorySessionStore::new();
        let mut session = store.get_or_create("slack", "kings", "allan").unwrap();
        session.add_user_message(&format!("token {GH_SECRET}"), None);
        store.save(&session).unwrap();

        let stats = store.redact_secrets_in_session(&session.id, true).unwrap();
        assert_eq!(stats.messages_redacted, 1);
        let got = store.get(&session.id).unwrap().unwrap();
        assert!(got.messages[0]
            .content
            .contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
    }

    #[test]
    fn redact_session_scrubs_jsonl_live_and_archive() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path()).unwrap();
        let mut session = store.get_or_create("slack", "kings", "allan").unwrap();
        let mid = session.add_user_message(&format!("here {GH_SECRET}"), None);
        store.save(&session).unwrap();
        let archived = session
            .messages
            .iter()
            .find(|m| m.id == mid)
            .unwrap()
            .clone();
        store.append_to_archive(&session.id, &archived).unwrap();

        let stats = store.redact_secrets_in_session(&session.id, false).unwrap();
        assert_eq!(stats.messages_redacted, 1);
        assert_eq!(stats.archive_messages_redacted, 1);

        let sessions_dir = dir.path().join("sessions");
        let live =
            std::fs::read_to_string(sessions_dir.join(format!("{}.jsonl", session.id))).unwrap();
        let archive =
            std::fs::read_to_string(sessions_dir.join(format!("{}.archive.jsonl", session.id)))
                .unwrap();
        assert!(!live.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
        assert!(!archive.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));

        let again = store.redact_secrets_in_session(&session.id, false).unwrap();
        assert_eq!(again.messages_redacted, 0);
        assert_eq!(again.archive_messages_redacted, 0);
    }

    #[test]
    fn jsonl_store_persists_to_disk() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");

        let session_id;
        {
            let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");
            let mut session = store
                .get_or_create("slack", "kings", "allan")
                .expect("get_or_create failed");
            session_id = session.id.clone();

            session.add_user_message("Hello!", None);
            session.add_assistant_message("Hi!", 10);
            store.save(&session).expect("save failed");
        }
        // Store dropped, re-create from same directory
        {
            let store = JsonlSessionStore::new(dir.path()).expect("failed to reload store");
            let loaded = store
                .get(&session_id)
                .expect("get failed")
                .expect("session not found after reload");
            assert_eq!(loaded.message_count(), 2);
            assert_eq!(loaded.channel, "slack");
            assert_eq!(loaded.owner, "kings");
            assert_eq!(loaded.messages[0].content, "Hello!");
            assert_eq!(loaded.messages[1].content, "Hi!");
        }
    }

    #[test]
    fn jsonl_store_lazy_loads_message_bodies_on_restart() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");

        let session_id = {
            let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");
            let mut session = store
                .get_or_create("slack", "kings", "allan")
                .expect("get_or_create failed");
            session.add_user_message("Hello!", None);
            session.add_assistant_message("Hi!", 10);
            store.save(&session).expect("save failed");
            session.id
        };

        let store = JsonlSessionStore::new(dir.path()).expect("failed to reload store");

        // Right after reload the cache holds a shell — meta only, no
        // message bodies — but `message_count()` already answers truthfully.
        let listed = store.list(None, 10).expect("list failed");
        assert_eq!(listed.len(), 1);
        let shell = &listed[0];
        assert!(shell.messages.is_empty());
        assert_eq!(shell.message_count(), 2);
        assert_eq!(shell.cached_message_count, Some(2));

        // `get()` hydrates on demand and clears the shell marker.
        let hydrated = store
            .get(&session_id)
            .expect("get failed")
            .expect("session not found");
        assert_eq!(hydrated.messages.len(), 2);
        assert_eq!(hydrated.message_count(), 2);
        assert!(hydrated.cached_message_count.is_none());
        assert_eq!(hydrated.messages[0].content, "Hello!");
        assert_eq!(hydrated.messages[1].content, "Hi!");
    }

    #[test]
    fn jsonl_store_recovers_count_for_legacy_meta_without_message_count() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");

        let mut session = store
            .get_or_create("slack", "kings", "allan")
            .expect("get_or_create failed");
        session.add_user_message("First", None);
        session.add_user_message("Second", None);
        session.add_user_message("Third", None);
        store.save(&session).expect("save failed");

        // Simulate a meta file written before `message_count` was added
        // by stripping the field. The lazy loader should rebuild the
        // count from the JSONL so the lifecycle scanner still sees it.
        let meta_path = dir
            .path()
            .join("sessions")
            .join(format!("{}.meta.json", session.id));
        let raw = std::fs::read_to_string(&meta_path).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        value.as_object_mut().unwrap().remove("message_count");
        std::fs::write(&meta_path, serde_json::to_string_pretty(&value).unwrap()).unwrap();

        let store = JsonlSessionStore::new(dir.path()).expect("failed to reload");
        let listed = store.list(None, 10).expect("list failed");
        assert_eq!(listed[0].message_count(), 3);
    }

    #[test]
    fn jsonl_store_save_after_list_does_not_truncate_disk() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let session_id = {
            let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");
            let mut session = store
                .get_or_create("slack", "kings", "allan")
                .expect("get_or_create failed");
            session.add_user_message("preserve me", None);
            session.add_assistant_message("and me", 5);
            store.save(&session).expect("save failed");
            session.id
        };

        // New process: session is a shell. A buggy caller mutates a
        // non-message field on the shell and saves — the JSONL must
        // survive, even though `messages` in the shell is empty.
        let store = JsonlSessionStore::new(dir.path()).expect("failed to reload");
        let mut shell = store
            .list(None, 10)
            .expect("list failed")
            .pop()
            .expect("session not found");
        shell.label = Some("renamed".to_string());
        store.save(&shell).expect("save failed");

        // Re-load via a fresh store and confirm the bodies are intact.
        let store = JsonlSessionStore::new(dir.path()).expect("failed to reload");
        let restored = store
            .get(&session_id)
            .expect("get failed")
            .expect("session not found");
        assert_eq!(restored.messages.len(), 2);
        assert_eq!(restored.messages[0].content, "preserve me");
        assert_eq!(restored.messages[1].content, "and me");
    }

    #[test]
    fn jsonl_store_list_sessions() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");

        store
            .get_or_create("slack", "kings", "allan")
            .expect("get_or_create failed");
        store
            .get_or_create("cli", "test", "allan")
            .expect("get_or_create failed");
        store
            .get_or_create("slack", "other", "allan")
            .expect("get_or_create failed");

        let all = store.list(None, 10).expect("list failed");
        assert_eq!(all.len(), 3);

        let slack_only = store.list(Some("slack"), 10).expect("list failed");
        assert_eq!(slack_only.len(), 2);

        let cli_only = store.list(Some("cli"), 10).expect("list failed");
        assert_eq!(cli_only.len(), 1);

        // Limit works
        let limited = store.list(None, 2).expect("list failed");
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn jsonl_store_delete_session() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");

        let mut session = store
            .get_or_create("slack", "kings", "allan")
            .expect("get_or_create failed");
        session.add_user_message("test", None);
        store.save(&session).expect("save failed");

        // Verify files exist
        assert!(store.meta_path(&session.id).exists());
        assert!(store.messages_path(&session.id).exists());

        // Delete
        store.delete(&session.id).expect("delete failed");

        // Gone from cache
        assert!(store.get(&session.id).expect("get failed").is_none());

        // Gone from disk
        assert!(!store.meta_path(&session.id).exists());
        assert!(!store.messages_path(&session.id).exists());
    }

    #[test]
    fn jsonl_store_survives_restart_with_multiple_sessions() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");

        let mut session_ids = Vec::new();
        {
            let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");
            for i in 0..5 {
                let mut session = store
                    .get_or_create(&format!("ch-{i}"), &format!("user-{i}"), "agent")
                    .expect("get_or_create failed");
                session.add_user_message(&format!("Message from user {i}"), None);
                session.add_assistant_message(&format!("Reply to user {i}"), 10);
                store.save(&session).expect("save failed");
                session_ids.push(session.id.clone());
            }
        }

        // Reload
        let store = JsonlSessionStore::new(dir.path()).expect("failed to reload store");
        let all = store.list(None, 100).expect("list failed");
        assert_eq!(all.len(), 5);

        for (i, sid) in session_ids.iter().enumerate() {
            let session = store
                .get(sid)
                .expect("get failed")
                .expect("session not found");
            assert_eq!(session.message_count(), 2);
            assert_eq!(session.channel, format!("ch-{i}"));
            assert_eq!(
                session.messages[0].content,
                format!("Message from user {i}")
            );
        }
    }

    #[test]
    fn rollover_metadata_survives_jsonl_reload() {
        let dir = tempfile::tempdir().expect("temp dir");
        let id = {
            let store = JsonlSessionStore::new(dir.path()).expect("store");
            let mut s = store.get_or_create("ios", "u", "agent").expect("create");
            s.set_seq(3);
            s.set_prev_session_id("prev-123");
            s.seal();
            store.save(&s).expect("save");
            s.id.clone()
        };
        // Fresh store reads metadata back from disk — no migration needed.
        let store = JsonlSessionStore::new(dir.path()).expect("reopen");
        let s = store.get(&id).expect("get").expect("present");
        assert!(s.is_sealed());
        assert_eq!(s.seq(), 3);
        assert_eq!(s.prev_session_id(), Some("prev-123"));
    }

    #[test]
    fn get_or_create_returns_newest_unsealed_across_reload() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("store");

        let mut s0 = store.get_or_create("ios", "u", "agent").expect("create");
        s0.add_user_message("first", None);
        s0.set_seq(0);
        s0.seal();
        store.save(&s0).expect("save");

        let mut s1 = Session::new("ios", "u", "agent");
        s1.set_seq(1);
        store.save(&s1).expect("save");

        let active = store.get_or_create("ios", "u", "agent").expect("resolve");
        assert_eq!(
            active.id, s1.id,
            "newest unsealed wins over the sealed predecessor"
        );
    }

    #[test]
    fn sealing_preserves_messages_on_reload() {
        let dir = tempfile::tempdir().expect("temp dir");
        let id;
        {
            let store = JsonlSessionStore::new(dir.path()).expect("store");
            let mut s = store.get_or_create("ios", "u", "agent").expect("create");
            for i in 0..6 {
                s.add_user_message(&format!("msg {i}"), None);
            }
            store.save(&s).expect("save");
            id = s.id.clone();
        }
        // Reopen, hydrate, seal, save — the rollover seal path. The sealed
        // session's messages must survive (it remains the readable transcript
        // segment for its thread).
        {
            let store = JsonlSessionStore::new(dir.path()).expect("reopen");
            let mut s = store.get(&id).expect("get").expect("present");
            s.seal();
            store.save(&s).expect("save");
        }
        let store = JsonlSessionStore::new(dir.path()).expect("reopen2");
        let s = store.get(&id).expect("get").expect("present");
        assert!(s.is_sealed());
        assert_eq!(s.message_count(), 6, "messages survive seal + reload");
    }

    #[test]
    fn is_voice_reads_metadata_flag() {
        let mut s = Session::new("ios", "u", "agent");
        assert!(!s.is_voice());
        s.metadata = serde_json::json!({ "voice_mode": true });
        assert!(s.is_voice());
        s.metadata = serde_json::json!({ "voice_mode": false });
        assert!(!s.is_voice());
    }

    #[test]
    fn is_voice_turn_is_independent_of_the_latched_session_flag() {
        let mut s = Session::new("ios", "u", "agent");
        assert!(!s.is_voice_turn());

        // A spoken turn sets both.
        s.metadata = serde_json::json!({ "voice_mode": true, "voice_turn": true });
        assert!(s.is_voice_turn());

        // Typing again in the same session clears only the per-turn flag —
        // the session stays a voice conversation for rollover/digest purposes.
        s.metadata = serde_json::json!({ "voice_mode": true, "voice_turn": false });
        assert!(s.is_voice());
        assert!(!s.is_voice_turn());
    }

    #[test]
    fn the_rich_blocks_rendered_are_rewritten_only_when_they_change() {
        use crate::rich::RichFeatures;
        let mut s = Session::new("desk", "u", "agent");
        assert!(s.rendered_rich().is_empty());
        assert!(!s.set_rendered_rich(RichFeatures::default()));

        let desk = RichFeatures::from_names(["tables,forms"]);
        assert!(s.set_rendered_rich(desk));
        assert_eq!(s.rendered_rich(), desk);
        assert!(!s.set_rendered_rich(desk), "unchanged");

        assert!(s.set_rendered_rich(RichFeatures::default()));
        assert!(s.rendered_rich().is_empty());
        assert!(s.metadata.get(RICH_RENDERED_KEY).is_none());
    }

    #[test]
    fn a_rollover_successor_keeps_rendering_the_rich_blocks() {
        use crate::rich::RichFeatures;
        let mut from = Session::new("desk", "u", "agent");
        from.set_rendered_rich(RichFeatures::ALL);
        let mut successor = Session::new("desk", "u", "agent");
        successor.adopt_conversation_mode(&from);
        assert_eq!(successor.rendered_rich(), RichFeatures::ALL);
    }

    #[test]
    fn adopt_conversation_mode_carries_the_current_turn_mode() {
        let mut from = Session::new("ios", "u", "agent");
        from.metadata = serde_json::json!({
            "voice_mode": true,
            "voice_turn": true,
            "tts_provider": "fishaudio",
        });
        let mut successor = Session::new("ios", "u", "agent");
        successor.adopt_conversation_mode(&from);

        assert!(successor.is_voice());
        assert!(successor.is_voice_turn());
        assert_eq!(
            successor
                .metadata
                .get("tts_provider")
                .and_then(|v| v.as_str()),
            Some("fishaudio")
        );
    }

    #[test]
    fn a_message_saved_before_desk_marks_reads_unchanged() {
        let stored = serde_json::json!({
            "id": "m1",
            "role": "assistant",
            "content": "hello",
            "timestamp": "2026-10-01T09:00:00Z",
            "tokens": 3,
        });
        let message: SessionMessage = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(message.desk, None);
        assert_eq!(serde_json::to_value(&message).unwrap(), stored);
    }

    #[test]
    fn a_desk_mark_round_trips_through_the_live_session_and_the_archive() {
        use crate::channels::desk::{DeliveryKind, DeskMark};
        let dir = tempfile::tempdir().expect("temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("store");
        let mut s = store.get_or_create("desk", "u", "agent").expect("create");
        let mark = DeskMark {
            room: Some(Uuid::new_v4()),
            thread: Some(Uuid::new_v4()),
            delivery: Some(DeliveryKind::Reminder),
            request_id: Some("r1".into()),
            ..DeskMark::default()
        };
        s.add_assistant_message("a reminder", 2);
        let message = s.messages.last_mut().unwrap();
        message.desk = Some(mark.clone());
        let message = message.clone();
        store.save(&s).expect("save");
        store.append_to_archive(&s.id, &message).expect("archive");

        let live = store.get(&s.id).unwrap().unwrap();
        assert_eq!(live.messages.last().unwrap().desk.as_ref(), Some(&mark));
        let page = store.read_archive_page(&s.id, None, 10).unwrap();
        assert_eq!(page.messages.last().unwrap().desk.as_ref(), Some(&mark));
    }

    #[test]
    fn search_sessions_finds_archived_detail_compacted_out_of_live() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("store");
        let mut s = store.get_or_create("ios", "u", "agent").expect("create");

        // A detailed original turn is recorded to the append-only archive...
        s.add_user_message("the platypus migration logistics", None);
        let archived = s.messages.last().unwrap().clone();
        store.append_to_archive(&s.id, &archived).expect("archive");

        // ...then compaction replaces the live messages with a summary, so the
        // original detail is gone from session.messages but kept in the archive.
        s.messages.clear();
        s.add_user_message("[Compaction Summary] we discussed some logistics", None);
        store.save(&s).expect("save");

        let results = store
            .search_sessions("platypus migration", 10)
            .expect("search");
        assert_eq!(
            results.len(),
            1,
            "drill-down finds the archived original detail"
        );
        assert!(results[0].excerpt.to_lowercase().contains("platypus"));

        // Prove it came from the archive, not the compacted live messages.
        let live = store.get(&s.id).expect("get").expect("present");
        assert!(!live.messages.iter().any(|m| m.content.contains("platypus")));
    }

    fn user_msg(id: &str, content: &str, ts: DateTime<Utc>) -> SessionMessage {
        SessionMessage {
            id: id.to_string(),
            role: MessageRole::User,
            content: content.to_string(),
            tool_data: None,
            timestamp: ts,
            tokens: 0,
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
            desk: None,
        }
    }

    #[test]
    fn search_terms_are_distinct_lowercased_whitespace_tokens() {
        assert_eq!(
            search_terms("  Superhub  BROWSER superhub "),
            vec!["superhub".to_string(), "browser".to_string()]
        );
        assert!(search_terms("    ").is_empty());
        // Punctuation stays inside a token so ids/URLs survive.
        assert_eq!(search_terms("DW-546"), vec!["dw-546".to_string()]);
    }

    #[test]
    fn search_ranks_multiword_matches_by_coordination_then_recency() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("store");
        let s = store.get_or_create("ios", "u", "agent").expect("create");
        let t0 = Utc::now() - chrono::Duration::minutes(10);
        let two = user_msg("a", "logging into superhub with the browser", t0);
        let one = user_msg(
            "b",
            "took a screenshot of superhub",
            t0 + chrono::Duration::minutes(5),
        );
        let zero = user_msg(
            "c",
            "completely unrelated chatter",
            t0 + chrono::Duration::minutes(6),
        );
        for m in [&two, &one, &zero] {
            store.append_to_archive(&s.id, m).expect("archive");
        }

        // No single message holds all three terms, yet the query still surfaces
        // its best matches — the regression returned nothing for exactly this.
        let results = store
            .search_sessions("superhub browser login", 10)
            .expect("search");
        assert_eq!(
            results
                .iter()
                .map(|r| r.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"],
            "two-term match ranks above one-term; zero-term excluded"
        );

        // A single-term query keeps pure recency order.
        let results = store.search_sessions("superhub", 10).expect("search");
        assert_eq!(
            results
                .iter()
                .map(|r| r.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a"]
        );
    }

    #[test]
    fn in_memory_search_ranks_multiword_by_coordination() {
        let store = InMemorySessionStore::new();
        let mut s = store.get_or_create("ios", "u", "agent").expect("create");
        let t0 = Utc::now();
        s.messages
            .push(user_msg("a", "logging into superhub with the browser", t0));
        s.messages.push(user_msg(
            "b",
            "took a screenshot of superhub",
            t0 + chrono::Duration::minutes(1),
        ));
        store.save(&s).expect("save");

        let results = store
            .search_sessions("superhub browser login", 10)
            .expect("search");
        assert_eq!(
            results
                .iter()
                .map(|r| r.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn find_returns_active_session_ignoring_sealed_and_matches_get_or_create() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("store");
        assert!(store.find("ios", "u").expect("find").is_none());

        let created = store.get_or_create("ios", "u", "agent").expect("create");
        let found = store.find("ios", "u").expect("find").expect("active");
        assert_eq!(found.id, created.id, "find resolves the same session");
        assert!(
            store.find("ios", "other").expect("find").is_none(),
            "a different owner is a different session"
        );

        let mut sealed = created;
        sealed.seal();
        store.save(&sealed).expect("save");
        assert!(
            store.find("ios", "u").expect("find").is_none(),
            "a sealed session is not active"
        );
    }

    fn chain_member(store: &JsonlSessionStore, seq: u64, prev: Option<&str>) -> Session {
        let mut s = Session::new("ios", "u", "agent");
        s.set_seq(seq);
        if let Some(p) = prev {
            s.set_prev_session_id(p);
        }
        store.save(&s).expect("save");
        s
    }

    fn archive_msgs(store: &JsonlSessionStore, sid: &str, tag: &str, n: usize) {
        for i in 0..n {
            let m = SessionMessage {
                id: format!("{tag}{i}"),
                role: MessageRole::User,
                content: format!("{tag} msg {i}"),
                tool_data: None,
                timestamp: Utc::now(),
                tokens: 1,
                external_id: None,
                attachments: None,
                trust_level: None,
                actor: None,
                desk: None,
            };
            store.append_to_archive(sid, &m).expect("archive");
        }
    }

    #[test]
    fn read_thread_page_spans_the_chain_chronologically() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("store");
        let a = chain_member(&store, 0, None);
        let b = chain_member(&store, 1, Some(&a.id));
        let c = chain_member(&store, 2, Some(&b.id));
        archive_msgs(&store, &a.id, "a", 3);
        archive_msgs(&store, &b.id, "b", 3);
        archive_msgs(&store, &c.id, "c", 3);

        let page = store
            .read_thread_page(&c.id, None, None, 100)
            .expect("page");
        let contents: Vec<&str> = page.messages.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(
            contents,
            vec![
                "a msg 0", "a msg 1", "a msg 2", "b msg 0", "b msg 1", "b msg 2", "c msg 0",
                "c msg 1", "c msg 2"
            ]
        );
        assert!(!page.has_more);
    }

    #[test]
    fn read_thread_page_cursor_crosses_seam_without_gaps() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("store");
        let a = chain_member(&store, 0, None);
        let b = chain_member(&store, 1, Some(&a.id));
        archive_msgs(&store, &a.id, "a", 2);
        archive_msgs(&store, &b.id, "b", 2);

        // Newest page (head b) returns b's turns; more remains in a.
        let p1 = store.read_thread_page(&b.id, None, None, 2).expect("p1");
        let c1: Vec<&str> = p1.messages.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(c1, vec!["b msg 0", "b msg 1"]);
        assert!(p1.has_more);

        // Continue from the oldest entry held, crossing into a — no gap/overlap.
        let oldest = &p1.messages[0];
        let p2 = store
            .read_thread_page(&b.id, Some(&oldest.session_id), Some(&oldest.id), 2)
            .expect("p2");
        let c2: Vec<&str> = p2.messages.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(c2, vec!["a msg 0", "a msg 1"]);
        assert!(!p2.has_more);
    }

    #[test]
    fn read_thread_page_stops_at_pruned_predecessor() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("store");
        // b's predecessor id points at a session that no longer exists.
        let b = chain_member(&store, 1, Some("pruned-session-id"));
        archive_msgs(&store, &b.id, "b", 2);
        let page = store
            .read_thread_page(&b.id, None, None, 100)
            .expect("page");
        let contents: Vec<&str> = page.messages.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(contents, vec!["b msg 0", "b msg 1"]);
        assert!(!page.has_more, "pruned predecessor ends the chain cleanly");
    }

    #[test]
    fn jsonl_store_handles_tool_data() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");

        let mut session = store
            .get_or_create("slack", "kings", "allan")
            .expect("get_or_create failed");

        // Add a message with tool_data
        let msg = SessionMessage {
            id: Uuid::new_v4().to_string(),
            role: MessageRole::Tool,
            content: "tool result".to_string(),
            tool_data: Some(serde_json::json!({"tool": "read_file", "path": "/tmp/test"})),
            timestamp: Utc::now(),
            tokens: 10,
            external_id: Some("ext-123".to_string()),
            attachments: None,
            trust_level: None,
            actor: None,
            desk: None,
        };
        session.messages.push(msg);
        store.save(&session).expect("save failed");

        // Reload from disk
        drop(store);
        let store = JsonlSessionStore::new(dir.path()).expect("failed to reload store");
        let loaded = store
            .get(&session.id)
            .expect("get failed")
            .expect("session not found");
        assert_eq!(loaded.messages[0].role, MessageRole::Tool);
        assert_eq!(
            loaded.messages[0].tool_data,
            Some(serde_json::json!({"tool": "read_file", "path": "/tmp/test"}))
        );
        assert_eq!(loaded.messages[0].external_id, Some("ext-123".to_string()));
    }

    #[test]
    fn jsonl_store_skips_malformed_lines() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");

        // Create a session normally first
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");
        let mut session = store
            .get_or_create("slack", "kings", "allan")
            .expect("get_or_create failed");
        session.add_user_message("Good message", None);
        store.save(&session).expect("save failed");

        // Corrupt the JSONL file by appending a bad line
        let jsonl_path = store.messages_path(&session.id);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl_path)
            .expect("failed to open jsonl");
        writeln!(file, "{{not valid json at all").expect("failed to write");
        writeln!(
            file,
            r#"{{"id":"msg-2","role":"user","content":"Second good message","timestamp":"2026-01-01T00:00:00Z","tokens":5}}"#
        )
        .expect("failed to write");
        drop(file);

        // Reload — should skip the bad line and load the good ones
        drop(store);
        let store = JsonlSessionStore::new(dir.path()).expect("failed to reload store");
        let loaded = store
            .get(&session.id)
            .expect("get failed")
            .expect("session not found");
        assert_eq!(loaded.message_count(), 2);
        assert_eq!(loaded.messages[0].content, "Good message");
        assert_eq!(loaded.messages[1].content, "Second good message");
    }

    #[test]
    fn jsonl_store_disk_files_have_correct_format() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");

        let mut session = store
            .get_or_create("slack", "kings", "allan")
            .expect("get_or_create failed");
        session.add_user_message("Hello!", None);
        session.add_assistant_message("Hi!", 10);
        store.save(&session).expect("save failed");

        // Verify meta.json is valid JSON with expected fields
        let meta_content =
            std::fs::read_to_string(store.meta_path(&session.id)).expect("failed to read meta");
        let meta: serde_json::Value =
            serde_json::from_str(&meta_content).expect("meta is not valid JSON");
        assert_eq!(meta["channel"], "slack");
        assert_eq!(meta["owner"], "kings");
        assert_eq!(meta["agent_id"], "allan");

        // Verify JSONL has one line per message
        let jsonl_content = std::fs::read_to_string(store.messages_path(&session.id))
            .expect("failed to read jsonl");
        let lines: Vec<&str> = jsonl_content.lines().collect();
        assert_eq!(lines.len(), 2);

        // Each line is valid JSON
        let msg1: serde_json::Value =
            serde_json::from_str(lines[0]).expect("line 1 is not valid JSON");
        assert_eq!(msg1["role"], "user");
        assert_eq!(msg1["content"], "Hello!");

        let msg2: serde_json::Value =
            serde_json::from_str(lines[1]).expect("line 2 is not valid JSON");
        assert_eq!(msg2["role"], "assistant");
        assert_eq!(msg2["content"], "Hi!");
    }

    #[test]
    fn jsonl_store_get_or_create_reloads_from_disk() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");

        // Create a session and save it
        {
            let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");
            let session = store
                .get_or_create("slack", "kings", "allan")
                .expect("get_or_create failed");
            assert_eq!(session.channel, "slack");
        }

        // Reload — get_or_create should find the session loaded from disk
        {
            let store = JsonlSessionStore::new(dir.path()).expect("failed to reload store");
            let session = store
                .get_or_create("slack", "kings", "allan")
                .expect("get_or_create failed");
            assert_eq!(session.channel, "slack");
            // Should be the same session, not a new one
            let all = store.list(None, 100).expect("list failed");
            assert_eq!(all.len(), 1);
        }
    }

    // ─── Transcript Tests ───

    /// Helper: create a session with a realistic conversation including tool calls.
    fn build_conversation_session(store: &dyn SessionStore) -> Session {
        let mut session = store
            .get_or_create("slack", "kings", "allan")
            .expect("get_or_create failed");

        session.add_system_message("You are Allan, a helpful AI assistant.");
        session.add_user_message("Read the file /tmp/test.txt", None);

        // Simulate tool call
        session.add_tool_message(
            "filesystem.read",
            &serde_json::json!({"path": "/tmp/test.txt"}),
            &serde_json::json!({"content": "Hello World"}),
            true,
            42,
            None,
        );

        session.add_assistant_message("The file contains: Hello World", 150);

        session.add_user_message("Now delete it", None);

        // Simulate a failed tool call
        session.add_tool_message(
            "filesystem.delete",
            &serde_json::json!({"path": "/tmp/test.txt"}),
            &serde_json::json!({"error": "Permission denied"}),
            false,
            5,
            None,
        );

        session.add_assistant_message("I couldn't delete the file — permission denied.", 120);

        store.save(&session).expect("save failed");
        session
    }

    #[test]
    fn transcript_read_reconstructs_conversation() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");
        let session = build_conversation_session(&store);

        let transcript = store
            .read_transcript(&session.id)
            .expect("read_transcript failed");

        assert_eq!(transcript.len(), 7);

        // Check roles
        assert_eq!(transcript[0].role, MessageRole::System);
        assert_eq!(transcript[1].role, MessageRole::User);
        assert_eq!(transcript[2].role, MessageRole::Tool);
        assert_eq!(transcript[3].role, MessageRole::Assistant);
        assert_eq!(transcript[4].role, MessageRole::User);
        assert_eq!(transcript[5].role, MessageRole::Tool);
        assert_eq!(transcript[6].role, MessageRole::Assistant);

        // Check session_id is set on every entry
        for entry in &transcript {
            assert_eq!(entry.session_id, session.id);
        }

        // Check tool metadata on tool entries
        let tool_entry = &transcript[2];
        let meta = tool_entry
            .tool_metadata
            .as_ref()
            .expect("expected tool metadata");
        assert_eq!(meta.name, "filesystem.read");
        assert!(meta.success);
        assert_eq!(meta.duration_ms, 42);

        let failed_tool = &transcript[5];
        let meta = failed_tool
            .tool_metadata
            .as_ref()
            .expect("expected tool metadata");
        assert_eq!(meta.name, "filesystem.delete");
        assert!(!meta.success);

        // Check token usage on assistant entries
        let asst_entry = &transcript[3];
        let usage = asst_entry
            .token_usage
            .as_ref()
            .expect("expected token usage");
        assert_eq!(usage.output_tokens, 150);
    }

    #[test]
    fn transcript_read_nonexistent_session_returns_error() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");

        let result = store.read_transcript("nonexistent-id");
        assert!(result.is_err());
    }

    #[test]
    fn session_export_contains_all_data() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");
        let session = build_conversation_session(&store);

        let export = store.export_session(&session.id).expect("export failed");

        // Metadata
        assert_eq!(export.id, session.id);
        assert_eq!(export.channel, "slack");
        assert_eq!(export.owner, "kings");
        assert_eq!(export.agent_id, "allan");

        // Transcript present
        assert_eq!(export.transcript.len(), 7);

        // Stats present
        assert_eq!(export.stats.message_count, 7);
        assert!(export.stats.tool_calls.total >= 2);

        // Serialises to valid JSON
        let json = serde_json::to_string_pretty(&export).expect("serialization failed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("deserialization failed");
        assert_eq!(parsed["channel"], "slack");
        assert!(parsed["transcript"].is_array());
        assert!(parsed["stats"]["tool_calls"]["total"].as_u64().unwrap_or(0) >= 2);
    }

    #[test]
    fn session_stats_message_count_and_role_breakdown() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");
        let session = build_conversation_session(&store);

        let stats = store.session_stats(&session.id).expect("stats failed");

        assert_eq!(stats.message_count, 7);
        assert_eq!(stats.messages_by_role.get("system"), Some(&1));
        assert_eq!(stats.messages_by_role.get("user"), Some(&2));
        assert_eq!(stats.messages_by_role.get("assistant"), Some(&2));
        assert_eq!(stats.messages_by_role.get("tool"), Some(&2));
    }

    #[test]
    fn session_stats_tool_call_summary() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");
        let session = build_conversation_session(&store);

        let stats = store.session_stats(&session.id).expect("stats failed");

        assert_eq!(stats.tool_calls.total, 2);
        assert_eq!(stats.tool_calls.succeeded, 1);
        assert_eq!(stats.tool_calls.failed, 1);
        assert_eq!(stats.tool_calls.total_duration_ms, 47); // 42 + 5

        // Per-tool breakdown
        let read_stats = stats
            .tool_calls
            .by_tool
            .get("filesystem.read")
            .expect("missing read stats");
        assert_eq!(read_stats.count, 1);
        assert_eq!(read_stats.succeeded, 1);
        assert_eq!(read_stats.total_duration_ms, 42);

        let delete_stats = stats
            .tool_calls
            .by_tool
            .get("filesystem.delete")
            .expect("missing delete stats");
        assert_eq!(delete_stats.count, 1);
        assert_eq!(delete_stats.failed, 1);
    }

    #[test]
    fn session_stats_token_breakdown() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");
        let session = build_conversation_session(&store);

        let stats = store.session_stats(&session.id).expect("stats failed");

        // Assistant tokens: 150 + 120 = 270
        assert_eq!(stats.total_output_tokens, 270);

        // Tokens by role should have assistant entry
        let asst_tokens = stats.tokens_by_role.get("assistant").copied().unwrap_or(0);
        assert_eq!(asst_tokens, 270);
    }

    #[test]
    fn session_stats_duration() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");
        let session = build_conversation_session(&store);

        let stats = store.session_stats(&session.id).expect("stats failed");

        // Duration should be non-negative (messages created nearly simultaneously in test)
        assert!(stats.duration_seconds.is_some());
        assert!(stats.duration_seconds.unwrap_or(-1) >= 0);
    }

    #[test]
    fn in_memory_store_transcript_operations() {
        let store = InMemorySessionStore::new();
        let session = build_conversation_session(&store);

        // read_transcript via trait
        let transcript = store
            .read_transcript(&session.id)
            .expect("read_transcript failed");
        assert_eq!(transcript.len(), 7);

        // export_session via trait
        let export = store.export_session(&session.id).expect("export failed");
        assert_eq!(export.transcript.len(), 7);
        assert_eq!(export.stats.message_count, 7);

        // session_stats via trait
        let stats = store.session_stats(&session.id).expect("stats failed");
        assert_eq!(stats.tool_calls.total, 2);
    }

    #[test]
    fn transcript_persists_across_restart() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let session_id;
        {
            let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");
            let session = build_conversation_session(&store);
            session_id = session.id.clone();
        }

        // Reload from disk
        let store = JsonlSessionStore::new(dir.path()).expect("failed to reload store");
        let transcript = store
            .read_transcript(&session_id)
            .expect("read_transcript failed");
        assert_eq!(transcript.len(), 7);

        let stats = store.session_stats(&session_id).expect("stats failed");
        assert_eq!(stats.tool_calls.total, 2);
        assert_eq!(stats.tool_calls.succeeded, 1);
        assert_eq!(stats.tool_calls.failed, 1);
    }

    #[test]
    fn add_tool_message_creates_structured_tool_data() {
        let mut session = Session::new("test", "user", "agent");
        session.add_tool_message(
            "exec.run",
            &serde_json::json!({"command": "ls -la"}),
            &serde_json::json!({"stdout": "total 0"}),
            true,
            100,
            None,
        );

        assert_eq!(session.message_count(), 1);
        let msg = &session.messages[0];
        assert_eq!(msg.role, MessageRole::Tool);

        let td = msg.tool_data.as_ref().expect("tool_data should be present");
        assert_eq!(td["name"], "exec.run");
        assert_eq!(td["success"], true);
        assert_eq!(td["duration_ms"], 100);
        assert!(
            td.get("error").is_none(),
            "successful tool calls must not carry an error field"
        );
    }

    #[test]
    fn add_tool_message_records_error_on_failure() {
        let mut session = Session::new("test", "user", "agent");
        session.add_tool_message(
            "github.pr_create",
            &serde_json::json!({"repo": "workforce"}),
            &serde_json::Value::Null,
            false,
            12,
            Some("GitHub token not configured at skills/github/token"),
        );
        let td = session.messages[0]
            .tool_data
            .as_ref()
            .expect("tool_data present");
        assert_eq!(td["success"], false);
        assert_eq!(
            td["error"],
            "GitHub token not configured at skills/github/token"
        );
    }

    #[test]
    fn add_tool_message_omits_error_when_empty() {
        let mut session = Session::new("test", "user", "agent");
        session.add_tool_message(
            "anything",
            &serde_json::Value::Null,
            &serde_json::Value::Null,
            false,
            1,
            Some(""),
        );
        let td = session.messages[0].tool_data.as_ref().unwrap();
        assert!(td.get("error").is_none());
    }

    #[test]
    fn add_tool_message_caps_bulky_input_at_storage_boundary() {
        let mut session = Session::new("test", "user", "agent");
        let big_rows: Vec<serde_json::Value> = (0..500)
            .map(|i| serde_json::json!([format!("row{i}"), i, "filler".repeat(20)]))
            .collect();

        session.add_tool_message(
            "sheets.append",
            &serde_json::json!({
                "sheet_id": "abc",
                "range": "Sheet1!A1",
                "values": big_rows,
            }),
            &serde_json::json!({"updatedRange": "Sheet1!A1:C500"}),
            true,
            42,
            None,
        );

        let td = session.messages[0]
            .tool_data
            .as_ref()
            .expect("tool_data present");
        assert_eq!(td["input"]["sheet_id"], "abc");
        assert_eq!(td["input"]["range"], "Sheet1!A1");
        assert!(
            td["input"]["values"].is_string(),
            "bulky values must be elided"
        );
    }

    fn delivery(tool: &str, input: serde_json::Value, success: bool) -> SessionMessage {
        SessionMessage::tool(
            tool,
            &input,
            &serde_json::json!({"ok": success}),
            success,
            12,
            None,
        )
    }

    #[test]
    fn a_delivery_row_carries_the_full_text_its_stored_input_elides() {
        let body = "every word of the drafted report ".repeat(200);
        let msg = delivery(
            "email.send",
            serde_json::json!({"to": "a@b.c", "subject": "Report", "body": body}),
            true,
        );
        assert_eq!(
            msg.content,
            format!(
                "Tool 'email.send' completed in 12ms\n\nReport\n\n{}",
                body.trim()
            )
        );
        assert_eq!(msg.headline(), "Tool 'email.send' completed in 12ms");
        let stored = &msg.tool_data.as_ref().expect("tool_data")["input"];
        assert!(
            stored["body"]
                .as_str()
                .is_some_and(|b| b.contains("elided")),
            "stored input is still shrunk: {stored}"
        );
    }

    #[test]
    fn a_failed_delivery_records_only_that_it_failed() {
        let msg = delivery(
            "messaging.send",
            serde_json::json!({"channel": "D1", "text": "never arrived"}),
            false,
        );
        assert_eq!(msg.content, "Tool 'messaging.send' failed after 12ms");
    }

    #[test]
    fn a_tool_that_delivers_nothing_keeps_its_one_line_content() {
        let msg = delivery("exec.run", serde_json::json!({"text": "ls"}), true);
        assert_eq!(msg.content, "Tool 'exec.run' completed in 12ms");
        assert_eq!(msg.headline(), msg.content);
    }

    #[test]
    fn secrets_in_delivered_text_are_redacted() {
        let token = format!("ghp_{}", "a1B2".repeat(10));
        let msg = delivery(
            "messaging.send",
            serde_json::json!({"channel": "D1", "text": format!("use {token} to push")}),
            true,
        );
        assert!(!msg.content.contains(&token), "{}", msg.content);
        assert!(msg.content.contains("to push"));
    }

    #[test]
    fn delivered_text_over_the_cap_is_truncated_with_an_ellipsis() {
        let msg = delivery(
            "messaging.upload_file",
            serde_json::json!({"content_text": "x".repeat(DELIVERED_TEXT_MAX_BYTES * 2)}),
            true,
        );
        assert_eq!(
            msg.content,
            format!(
                "Tool 'messaging.upload_file' completed in 12ms\n\n{}…",
                "x".repeat(DELIVERED_TEXT_MAX_BYTES)
            )
        );
    }

    #[test]
    fn pruning_a_long_delivery_row_never_raises_the_token_count() {
        let mut session = Session::new("test", "user", "agent");
        session.push_message(delivery(
            "messaging.send",
            serde_json::json!({"channel": "D1", "text": "y".repeat(10_000)}),
            true,
        ));
        let before = session.total_tokens;
        assert_eq!(session.prune_tool_outputs(2000, 1024, 0), 1);
        assert!(session.messages[0].content.contains("[truncated, was"));
        assert_eq!(session.messages[0].tokens, 0);
        assert_eq!(session.total_tokens, before);
    }

    /// A session holding a word only a sent message contains, so a search
    /// for it can only hit the tool row.
    fn session_with_a_sent_word(store: &dyn SessionStore) -> (Session, String) {
        let mut session = store
            .get_or_create("slack:D1", "u", "agent")
            .expect("create");
        session.add_user_message("tell Sam about the fruit", None);
        let id = session.add_tool_message(
            "messaging.send",
            &serde_json::json!({"channel": "D1", "text": "the kumquat order shipped"}),
            &serde_json::json!({"ok": true}),
            true,
            12,
            None,
        );
        session.add_assistant_message("done", 1);
        store.save(&session).expect("save");
        (session, id)
    }

    fn assert_sent_word_found_and_read(store: &dyn SessionStore, session_id: &str, id: &str) {
        let hits = store.search_sessions("kumquat", 10).expect("search");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(
            (hits[0].session_id.as_str(), hits[0].message_id.as_str()),
            (session_id, id)
        );
        assert_eq!(hits[0].role, MessageRole::Tool);
        assert!(hits[0]
            .excerpt
            .contains("» [tool] Tool 'messaging.send' completed in 12ms"));
        assert!(hits[0].excerpt.contains("the kumquat order shipped"));
        let read = store
            .read_archive_message(session_id, id)
            .expect("read")
            .expect("hit is readable");
        assert!(read.content.ends_with("\n\nthe kumquat order shipped"));
        assert_eq!(read.role, MessageRole::Tool);
    }

    #[test]
    fn in_memory_search_finds_a_sent_message_and_reads_it_back() {
        let store = InMemorySessionStore::new();
        let (session, id) = session_with_a_sent_word(&store);
        assert_sent_word_found_and_read(&store, &session.id, &id);
        assert!(store
            .read_archive_message(&session.id, "nope")
            .unwrap()
            .is_none());
        assert!(store.read_archive_message("nope", &id).unwrap().is_none());
    }

    #[test]
    fn jsonl_search_finds_a_sent_message_and_reads_it_back_from_the_live_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("store");
        let (session, id) = session_with_a_sent_word(&store);
        assert_sent_word_found_and_read(&store, &session.id, &id);
        assert!(store
            .read_archive_message(&session.id, "nope")
            .unwrap()
            .is_none());
        assert!(store
            .read_archive_message("../nope", &id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn jsonl_reads_an_archived_delivery_compacted_out_of_the_live_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("store");
        let (mut session, id) = session_with_a_sent_word(&store);
        for msg in &session.messages {
            store.append_to_archive(&session.id, msg).expect("archive");
        }
        session.messages.clear();
        session.add_user_message("[Compaction Summary] fruit was discussed", None);
        store.save(&session).expect("save");
        assert_sent_word_found_and_read(&store, &session.id, &id);
    }

    #[test]
    fn add_tool_message_preserves_small_input_verbatim() {
        let mut session = Session::new("test", "user", "agent");
        let input = serde_json::json!({"command": "ls -la", "cwd": "/tmp"});
        session.add_tool_message(
            "exec.run",
            &input,
            &serde_json::json!({"stdout": "ok"}),
            true,
            5,
            None,
        );

        let td = session.messages[0]
            .tool_data
            .as_ref()
            .expect("tool_data present");
        assert_eq!(td["input"], input);
    }

    #[test]
    fn prune_tool_outputs_shrinks_input_too() {
        let mut session = Session::new("test", "user", "agent");
        let big_rows: Vec<serde_json::Value> = (0..500).map(|i| serde_json::json!([i])).collect();

        for _ in 0..5 {
            session.add_tool_message(
                "sheets.append",
                &serde_json::json!({"sheet_id": "a", "values": big_rows.clone()}),
                &serde_json::json!({"updatedRange": "A1"}),
                true,
                1,
                None,
            );
        }
        // Force input verbatim on disk for the test (bypass storage cap so we
        // can prove the prune path itself shrinks it).
        for msg in &mut session.messages {
            if let Some(ref mut td) = msg.tool_data {
                if let Some(obj) = td.as_object_mut() {
                    obj.insert(
                        "input".to_string(),
                        serde_json::json!({"sheet_id": "a", "values": big_rows.clone()}),
                    );
                }
            }
        }

        let pruned = session.prune_tool_outputs(2000, 256, 1);
        assert!(pruned > 0);

        for msg in &session.messages[..session.messages.len().saturating_sub(1)] {
            if let Some(ref td) = msg.tool_data {
                assert!(
                    td["input"]["values"].is_string(),
                    "older input.values should be elided, got {}",
                    td["input"]
                );
            }
        }
    }

    #[test]
    fn prune_tool_outputs_counts_messages_once_when_both_io_shrunk() {
        let mut session = Session::new("test", "user", "agent");
        let big_rows: Vec<serde_json::Value> = (0..500).map(|i| serde_json::json!([i])).collect();
        let big_output = "x".repeat(5000);

        session.add_tool_message(
            "sheets.append",
            &serde_json::json!({"sheet_id": "a", "values": big_rows.clone()}),
            &serde_json::json!({"echo": big_output}),
            true,
            1,
            None,
        );
        session.add_user_message("anchor", None);
        session.add_user_message("anchor", None);
        if let Some(ref mut td) = session.messages[0].tool_data {
            if let Some(obj) = td.as_object_mut() {
                obj.insert(
                    "input".to_string(),
                    serde_json::json!({"sheet_id": "a", "values": big_rows}),
                );
            }
        }

        let pruned = session.prune_tool_outputs(500, 256, 1);
        assert_eq!(
            pruned, 1,
            "single message must count once even when both io shrunk"
        );
    }

    #[test]
    fn compaction_config_default_includes_prune_tool_input_max_chars() {
        let cfg: CompactionConfig =
            serde_json::from_value(serde_json::json!({})).expect("defaults parse");
        assert_eq!(cfg.prune_tool_input_max_chars, 1024);
    }

    #[test]
    fn empty_session_stats() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = JsonlSessionStore::new(dir.path()).expect("failed to create store");

        let session = store
            .get_or_create("slack", "test", "allan")
            .expect("get_or_create failed");

        let stats = store.session_stats(&session.id).expect("stats failed");
        assert_eq!(stats.message_count, 0);
        assert_eq!(stats.tool_calls.total, 0);
        assert!(stats.duration_seconds.is_none());
    }

    // ── model_override tests ────────────────────────────────────────

    #[test]
    fn model_override_set_and_clear() {
        let mut session = Session::new("test", "user", "agent");
        assert!(session.model_override.is_none());

        session.set_model_override(Some("claude-opus-4".to_string()));
        assert_eq!(session.model_override.as_deref(), Some("claude-opus-4"));

        session.set_model_override(None);
        assert!(session.model_override.is_none());
    }

    #[test]
    fn model_override_serde_roundtrip() {
        let mut session = Session::new("test", "user", "agent");
        session.set_model_override(Some("claude-sonnet-4".to_string()));

        let json = serde_json::to_string(&session).unwrap();
        let deserialized: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(
            deserialized.model_override.as_deref(),
            Some("claude-sonnet-4")
        );
    }

    #[test]
    fn model_override_absent_in_old_sessions() {
        // Simulates loading a session serialized before model_override existed —
        // the field should default to None thanks to #[serde(default)].
        let session = Session::new("test", "user", "agent");
        let json = serde_json::to_string(&session).unwrap();

        // skip_serializing_if = "Option::is_none" keeps it out of JSON
        assert!(!json.contains("model_override"));

        let deserialized: Session = serde_json::from_str(&json).unwrap();
        assert!(deserialized.model_override.is_none());
    }

    // ─── Eviction Tests ───

    /// Helper: create a session with several tool messages for eviction testing.
    fn build_eviction_session() -> Session {
        let mut session = Session::new("test", "user", "agent");
        session.add_system_message("System prompt.");

        // 5 code.read tool messages with large outputs
        for i in 0..5 {
            let path = format!("src/file_{}.rs", i);
            let output = "x".repeat(2000); // 2000 chars each
            session.add_tool_message(
                "code.read",
                &serde_json::json!({"path": path}),
                &serde_json::json!(output),
                true,
                10,
                None,
            );
            // Manually set tokens for predictable testing
            let last = session.messages.last_mut().unwrap();
            let tokens = estimate_tokens(&last.content) + estimate_tokens(&output);
            last.tokens = tokens;
            session.total_tokens += tokens as u64;
        }

        // 2 code.search messages
        for i in 0..2 {
            let output = "y".repeat(1500);
            session.add_tool_message(
                "code.search",
                &serde_json::json!({"pattern": format!("pattern_{}", i)}),
                &serde_json::json!(output),
                true,
                5,
                None,
            );
            let last = session.messages.last_mut().unwrap();
            let tokens = estimate_tokens(&last.content) + estimate_tokens(&output);
            last.tokens = tokens;
            session.total_tokens += tokens as u64;
        }

        // 1 assistant message
        session.add_assistant_message("Here's what I found.", 20);

        session
    }

    // ── CCR (Phase 1): reversible tool-output retrieval ─────────────────────

    fn evicted_msgs<'a>(session: &'a Session, tool: &str) -> Vec<&'a serde_json::Value> {
        session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .filter_map(|m| m.tool_data.as_ref())
            .filter(|td| td.get("name").and_then(|v| v.as_str()) == Some(tool))
            .filter(|td| {
                td.get("output")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s.starts_with("[Output evicted"))
            })
            .collect()
    }

    #[test]
    fn evict_stashes_pristine_output_and_embeds_matching_handle() {
        let mut session = build_eviction_session();
        session.evict_tool_outputs(Some(&["code.read".to_string()]), None, 0, 500);

        let evicted = evicted_msgs(&session, "code.read");
        assert_eq!(evicted.len(), 5);
        for td in evicted {
            // Pristine original preserved verbatim under output_full.
            assert_eq!(
                td.get("output_full").and_then(|v| v.as_str()).map(str::len),
                Some(2000),
                "output_full must hold the pristine 2000-char original"
            );
            // The marker the model sees points at the handle, and it matches.
            let handle = td.get("retrieve_handle").and_then(|v| v.as_str()).unwrap();
            let marker = td.get("output").and_then(|v| v.as_str()).unwrap();
            assert!(
                marker.contains(&format!("tool.retrieve(handle=\"{handle}\")")),
                "marker must embed the recovery handle, got: {marker}"
            );
        }
    }

    #[test]
    fn prune_then_evict_preserves_the_pristine_original_idempotently() {
        let mut session = build_eviction_session();
        // First destructive op: prune truncates code.read outputs to 300 chars.
        session.prune_tool_outputs(300, 256, 0);
        let after_prune: Vec<String> = session
            .messages
            .iter()
            .filter_map(|m| m.tool_data.as_ref())
            .filter(|td| td.get("name").and_then(|v| v.as_str()) == Some("code.read"))
            .map(|td| {
                td.get("output_full")
                    .and_then(|v| v.as_str())
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(after_prune.len(), 5);
        assert!(after_prune.iter().all(|s| s.len() == 2000));

        // A later eviction pass must NOT overwrite the already-captured original
        // (idempotency keyed on retrieve_handle presence).
        session.evict_tool_outputs(Some(&["code.read".to_string()]), None, 0, 0);
        for (i, td) in session
            .messages
            .iter()
            .filter_map(|m| m.tool_data.as_ref())
            .filter(|td| td.get("name").and_then(|v| v.as_str()) == Some("code.read"))
            .enumerate()
        {
            assert_eq!(
                td.get("output_full").and_then(|v| v.as_str()).unwrap(),
                after_prune[i],
                "second destructive pass must preserve the first pristine capture"
            );
        }
    }

    #[test]
    fn stash_skips_when_over_session_byte_cap() {
        // Three ~800KB outputs: the cap (2MB) admits the first two, rejects the third.
        let mut session = Session::new("t", "u", "agent");
        for i in 0..3 {
            session.add_tool_message(
                "code.read",
                &serde_json::json!({ "path": format!("f{i}") }),
                &serde_json::json!("z".repeat(800_000)),
                true,
                1,
                None,
            );
        }
        session.evict_tool_outputs(Some(&["code.read".to_string()]), None, 0, 1000);

        let stashed = session
            .messages
            .iter()
            .filter_map(|m| m.tool_data.as_ref())
            .filter(|td| td.get("output_full").is_some())
            .count();
        assert_eq!(stashed, 2, "byte cap must bound stash growth to 2 of 3");
        // The unstashed one still got a (lossy) marker, with no recovery handle.
        let no_handle = session
            .messages
            .iter()
            .filter_map(|m| m.tool_data.as_ref())
            .filter(|td| td.get("retrieve_handle").is_none())
            .filter(|td| {
                td.get("output")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s.starts_with("[Output evicted"))
            })
            .count();
        assert_eq!(no_handle, 1);
    }

    #[test]
    fn test_evict_tool_outputs_basic() {
        let mut session = build_eviction_session();
        let tokens_before = session.total_tokens;

        // Evict code.read outputs, keep last 2
        let result = session.evict_tool_outputs(Some(&["code.read".to_string()]), None, 2, 500);

        assert_eq!(result.evicted, 3); // 5 total - 2 kept = 3 evicted
        assert_eq!(result.kept, 2);
        assert!(result.tokens_freed > 0);
        assert!(session.total_tokens < tokens_before);
    }

    #[test]
    fn test_evict_tool_outputs_by_tool_name() {
        let mut session = build_eviction_session();

        // Only evict code.read, code.search should be untouched
        let result = session.evict_tool_outputs(Some(&["code.read".to_string()]), None, 0, 500);

        assert_eq!(result.evicted, 5); // All 5 code.read messages
        assert_eq!(result.kept, 0);

        // Verify code.search messages are untouched
        for msg in &session.messages {
            if msg.role == MessageRole::Tool {
                if let Some(td) = &msg.tool_data {
                    if td.get("name").and_then(|v| v.as_str()) == Some("code.search") {
                        let output = td.get("output").and_then(|v| v.as_str()).unwrap_or("");
                        assert!(
                            !output.starts_with("[Output evicted"),
                            "code.search should not be evicted"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_evict_tool_outputs_keep_last() {
        let mut session = build_eviction_session();

        // keep_last=3 for code.read: only 2 should be evicted
        let result = session.evict_tool_outputs(Some(&["code.read".to_string()]), None, 3, 500);

        assert_eq!(result.evicted, 2);
        assert_eq!(result.kept, 3);

        // Verify the last 3 code.read messages still have original outputs
        let code_read_msgs: Vec<_> = session
            .messages
            .iter()
            .filter(|m| {
                m.role == MessageRole::Tool
                    && m.tool_data
                        .as_ref()
                        .and_then(|td| td.get("name"))
                        .and_then(|v| v.as_str())
                        == Some("code.read")
            })
            .collect();

        assert_eq!(code_read_msgs.len(), 5);
        // Last 3 should not be evicted
        for msg in &code_read_msgs[2..] {
            let output = msg
                .tool_data
                .as_ref()
                .and_then(|td| td.get("output"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            assert!(
                !output.starts_with("[Output evicted"),
                "Last 3 code.read messages should be preserved"
            );
        }
    }

    #[test]
    fn test_evict_tool_outputs_min_chars_threshold() {
        let mut session = Session::new("test", "user", "agent");

        // Add a tool message with small output (below threshold)
        session.add_tool_message(
            "code.read",
            &serde_json::json!({"path": "small.rs"}),
            &serde_json::json!("small output"),
            true,
            5,
            None,
        );
        let last = session.messages.last_mut().unwrap();
        last.tokens = 10;
        session.total_tokens += 10;

        // Add a tool message with large output (above threshold)
        let large_output = "z".repeat(1000);
        session.add_tool_message(
            "code.read",
            &serde_json::json!({"path": "large.rs"}),
            &serde_json::json!(large_output),
            true,
            5,
            None,
        );
        let last = session.messages.last_mut().unwrap();
        last.tokens = 260;
        session.total_tokens += 260;

        let result = session.evict_tool_outputs(None, None, 0, 500);

        // Only the large output should be evicted (small is below min_chars)
        assert_eq!(result.evicted, 1);
    }

    #[test]
    fn test_evict_tool_outputs_round_filter() {
        let mut session = build_eviction_session();

        // Messages: [0]=system, [1..5]=code.read, [6..7]=code.search, [8]=assistant
        // Only evict code.read outputs before round 4 (messages at indices < 4)
        let result = session.evict_tool_outputs(Some(&["code.read".to_string()]), Some(4), 0, 500);

        // Indices 1,2,3 are code.read before round 4 = 3 evicted
        assert_eq!(result.evicted, 3);
    }

    #[test]
    fn test_evict_tool_outputs_token_recalculation() {
        let mut session = build_eviction_session();
        let tokens_before = session.total_tokens;

        let result = session.evict_tool_outputs(None, None, 0, 500);

        // All 7 tool messages should be evicted (5 code.read + 2 code.search)
        assert_eq!(result.evicted, 7);

        // Verify total_tokens was reduced by tokens_freed
        assert_eq!(session.total_tokens, tokens_before - result.tokens_freed);

        // Verify each evicted message has updated tokens
        for msg in &session.messages {
            if msg.role == MessageRole::Tool && msg.content.starts_with("[Output evicted") {
                // Tokens should match the marker content
                assert_eq!(msg.tokens, estimate_tokens(&msg.content));
            }
        }
    }

    #[test]
    fn test_evict_tool_outputs_marker_format() {
        let mut session = Session::new("test", "user", "agent");
        let output = "a".repeat(800);
        session.add_tool_message(
            "code.read",
            &serde_json::json!({"path": "src/main.rs"}),
            &serde_json::json!(output),
            true,
            10,
            None,
        );
        let last = session.messages.last_mut().unwrap();
        last.tokens = 200;
        session.total_tokens += 200;

        session.evict_tool_outputs(None, None, 0, 500);

        let msg = &session.messages[0];
        assert!(msg.content.contains("Output evicted"));
        assert!(msg.content.contains("code.read"));
        assert!(msg.content.contains("src/main.rs"));
        // Should contain the original output size
        assert!(msg.content.contains("802")); // 800 chars + 2 for JSON quotes
    }

    #[test]
    fn test_evict_tool_outputs_no_match() {
        let mut session = build_eviction_session();

        // Filter for a tool that doesn't exist
        let result =
            session.evict_tool_outputs(Some(&["nonexistent.tool".to_string()]), None, 0, 500);

        assert_eq!(result.evicted, 0);
        assert_eq!(result.kept, 0);
        assert_eq!(result.tokens_freed, 0);
    }

    #[test]
    fn test_evict_by_round() {
        let mut session = build_eviction_session();
        let tokens_before = session.total_tokens;

        // Evict all tool outputs before round 6 (indices 0-5)
        // Messages: [0]=system, [1..5]=code.read, [6..7]=code.search, [8]=assistant
        let result = session.evict_by_round(6, 500);

        // Indices 1-5 are code.read tools (5 evicted), index 6+ are after boundary
        assert_eq!(result.evicted, 5);
        assert!(result.tokens_freed > 0);
        assert!(session.total_tokens < tokens_before);
    }

    #[test]
    fn test_evict_preserves_non_tool_messages() {
        let mut session = build_eviction_session();
        let system_content = session.messages[0].content.clone();
        let assistant_content = session.messages.last().unwrap().content.clone();

        session.evict_tool_outputs(None, None, 0, 500);

        // System message preserved
        assert_eq!(session.messages[0].content, system_content);
        assert_eq!(session.messages[0].role, MessageRole::System);

        // Assistant message preserved
        let last = session.messages.last().unwrap();
        assert_eq!(last.content, assistant_content);
        assert_eq!(last.role, MessageRole::Assistant);
    }

    #[test]
    fn paginate_entries_last_page() {
        let entries: Vec<TranscriptEntry> = (0..10)
            .map(|i| TranscriptEntry {
                id: format!("msg-{i}"),
                session_id: "s1".into(),
                timestamp: chrono::Utc::now(),
                role: MessageRole::User,
                content: format!("message {i}"),
                tool_metadata: None,
                token_usage: None,
                desk: None,
            })
            .collect();

        let page = paginate_entries(entries.clone(), None, 3).unwrap();
        assert_eq!(page.messages.len(), 3);
        assert_eq!(page.messages[0].id, "msg-7");
        assert_eq!(page.messages[2].id, "msg-9");
        assert!(page.has_more);
    }

    #[test]
    fn paginate_entries_with_cursor() {
        let entries: Vec<TranscriptEntry> = (0..10)
            .map(|i| TranscriptEntry {
                id: format!("msg-{i}"),
                session_id: "s1".into(),
                timestamp: chrono::Utc::now(),
                role: MessageRole::User,
                content: format!("message {i}"),
                tool_metadata: None,
                token_usage: None,
                desk: None,
            })
            .collect();

        let page = paginate_entries(entries.clone(), Some("msg-5"), 3).unwrap();
        assert_eq!(page.messages.len(), 3);
        assert_eq!(page.messages[0].id, "msg-2");
        assert_eq!(page.messages[2].id, "msg-4");
        assert!(page.has_more);
    }

    #[test]
    fn paginate_entries_first_page() {
        let entries: Vec<TranscriptEntry> = (0..5)
            .map(|i| TranscriptEntry {
                id: format!("msg-{i}"),
                session_id: "s1".into(),
                timestamp: chrono::Utc::now(),
                role: MessageRole::User,
                content: format!("message {i}"),
                tool_metadata: None,
                token_usage: None,
                desk: None,
            })
            .collect();

        let page = paginate_entries(entries, Some("msg-2"), 5).unwrap();
        assert_eq!(page.messages.len(), 2);
        assert_eq!(page.messages[0].id, "msg-0");
        assert_eq!(page.messages[1].id, "msg-1");
        assert!(!page.has_more);
    }

    #[test]
    fn paginate_entries_unknown_cursor_returns_all() {
        let entries: Vec<TranscriptEntry> = (0..3)
            .map(|i| TranscriptEntry {
                id: format!("msg-{i}"),
                session_id: "s1".into(),
                timestamp: chrono::Utc::now(),
                role: MessageRole::User,
                content: format!("message {i}"),
                tool_metadata: None,
                token_usage: None,
                desk: None,
            })
            .collect();

        let page = paginate_entries(entries, Some("nonexistent"), 10).unwrap();
        assert_eq!(page.messages.len(), 3);
        assert!(!page.has_more);
    }

    #[test]
    fn session_message_round_trips_without_trust_level() {
        let row = r#"{"id":"m1","role":"user","content":"hi","timestamp":"2026-05-22T10:00:00Z","tokens":3}"#;
        let msg: SessionMessage = serde_json::from_str(row).unwrap();
        assert_eq!(msg.trust_level, None);
        let serialized = serde_json::to_string(&msg).unwrap();
        assert!(!serialized.contains("trust_level"));
    }

    #[test]
    fn session_message_round_trips_with_trust_level() {
        use crate::trust::TrustLevel;
        let msg = SessionMessage {
            id: "m1".into(),
            role: MessageRole::User,
            content: "hi".into(),
            tool_data: None,
            timestamp: chrono::Utc::now(),
            tokens: 3,
            external_id: None,
            attachments: None,
            trust_level: Some(TrustLevel::Paired),
            actor: None,
            desk: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""trust_level":"paired""#));
        let back: SessionMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.trust_level, Some(TrustLevel::Paired));
    }

    #[test]
    fn add_user_message_with_trust_records_classification() {
        use crate::trust::TrustLevel;
        let mut session = Session::new("c", "u", "agent");
        session.add_user_message_with_trust("hi", None, None, Some(TrustLevel::Paired));
        let last = session.messages.last().unwrap();
        assert_eq!(last.trust_level, Some(TrustLevel::Paired));
        assert_eq!(last.content, "hi");
    }

    #[test]
    fn is_shared_conversation_detects_sentinel_owner() {
        assert!(Session::new("c", "shared:1700000000.0001", "agent").is_shared_conversation());
        assert!(!Session::new("c", "U123", "agent").is_shared_conversation());
    }

    #[test]
    fn attribute_messages_from_stamps_user_and_tool_only() {
        let mut session = Session::new("c", "shared:t", "agent");
        let base = session.messages.len();
        session.add_user_message("hello", None);
        session.add_tool_message(
            "datadog.logs",
            &serde_json::json!({}),
            &serde_json::json!("rows"),
            true,
            5,
            None,
        );
        session.add_assistant_message("here you go", 3);
        session.attribute_messages_from(base, "alice");

        let by_role = |role: MessageRole| {
            session
                .messages
                .iter()
                .filter(move |m| m.role == role)
                .map(|m| m.actor.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(by_role(MessageRole::User), vec![Some("alice".to_string())]);
        assert_eq!(by_role(MessageRole::Tool), vec![Some("alice".to_string())]);
        assert_eq!(by_role(MessageRole::Assistant), vec![None]);
    }

    #[test]
    fn attribute_messages_from_respects_slice_boundary() {
        let mut session = Session::new("c", "shared:t", "agent");
        session.add_user_message("first", None);
        let base = session.messages.len();
        session.add_user_message("second", None);
        session.attribute_messages_from(base, "bob");
        assert_eq!(session.messages[0].actor, None);
        assert_eq!(session.messages[1].actor, Some("bob".to_string()));
    }

    #[test]
    fn actor_absent_in_legacy_transcript_deserializes_to_none() {
        let legacy = serde_json::json!({
            "id": "m1",
            "role": "user",
            "content": "hi",
            "timestamp": "2026-01-01T00:00:00Z",
            "tokens": 1
        });
        let msg: SessionMessage = serde_json::from_value(legacy).unwrap();
        assert_eq!(msg.actor, None);
        // And a stamped actor survives a serialization round-trip.
        let stamped = SessionMessage {
            actor: Some("alice".to_string()),
            ..msg
        };
        let back: SessionMessage =
            serde_json::from_str(&serde_json::to_string(&stamped).unwrap()).unwrap();
        assert_eq!(back.actor, Some("alice".to_string()));
    }

    fn session_with_message_tokens(tokens: u32) -> Session {
        let mut session = Session::new("c", "u", "agent");
        session.add_user_message(&"x".repeat(tokens as usize * 4), None);
        session
    }

    fn config_with_max(max_context_tokens: u32) -> CompactionConfig {
        CompactionConfig {
            max_context_tokens,
            compaction_threshold: 0.5,
            urgent_compaction_threshold: 0.8,
            ..CompactionConfig::default()
        }
    }

    #[test]
    fn reserved_context_total_saturates() {
        let reserved = ReservedContext {
            system_prompt_tokens: u32::MAX,
            tool_schema_tokens: u32::MAX,
            response_reservation_tokens: u32::MAX,
        };
        assert_eq!(reserved.total(), u32::MAX);
    }

    #[test]
    fn with_prompt_tokens_adds_to_the_floor() {
        let reserved = ReservedContext {
            system_prompt_tokens: 1_000,
            tool_schema_tokens: 500,
            response_reservation_tokens: 200,
        };
        let widened = reserved.with_prompt_tokens(300);
        assert_eq!(widened.total(), reserved.total() + 300);
        assert_eq!(widened.tool_schema_tokens, 500);
        assert_eq!(widened.response_reservation_tokens, 200);
        assert_eq!(
            reserved.with_prompt_tokens(u32::MAX).system_prompt_tokens,
            u32::MAX
        );
    }

    #[test]
    fn needs_compaction_without_overhead_below_threshold() {
        let session = session_with_message_tokens(30_000);
        let config = config_with_max(131_072);
        assert!(!session.needs_compaction(&config, ReservedContext::default()));
    }

    #[test]
    fn needs_compaction_with_overhead_crosses_threshold() {
        let session = session_with_message_tokens(30_000);
        let config = config_with_max(131_072);
        let reserved = ReservedContext {
            system_prompt_tokens: 31_000,
            tool_schema_tokens: 16_000,
            response_reservation_tokens: 8_192,
        };
        assert!(session.needs_compaction(&config, reserved));
    }

    #[test]
    fn needs_urgent_compaction_accounts_for_overhead() {
        let session = session_with_message_tokens(60_000);
        let config = config_with_max(131_072);
        let reserved = ReservedContext {
            system_prompt_tokens: 31_000,
            tool_schema_tokens: 16_000,
            response_reservation_tokens: 0,
        };
        assert!(session.needs_urgent_compaction(&config, reserved));
        assert!(!session.needs_urgent_compaction(&config, ReservedContext::default()));
    }

    #[test]
    fn estimated_total_with_reserved_saturates() {
        let session = session_with_message_tokens(10);
        let reserved = ReservedContext {
            system_prompt_tokens: u32::MAX,
            tool_schema_tokens: 0,
            response_reservation_tokens: 0,
        };
        assert_eq!(session.estimated_total_with_reserved(reserved), u32::MAX);
    }

    #[test]
    fn conversational_message_count_excludes_system_and_tool() {
        let mut session = Session::new("ios", "u", "agent");
        session.set_system_message("system prompt");
        session.add_user_message("hi", None);
        session.add_assistant_message("hello", 2);
        session.add_tool_message(
            "read_file",
            &serde_json::json!({"path": "x"}),
            &serde_json::json!("output"),
            true,
            10,
            None,
        );
        assert_eq!(session.conversational_message_count(), 2);
    }

    #[test]
    fn observed_input_tokens_drive_compaction_when_estimate_is_blind() {
        let mut session = session_with_message_tokens(30_000);
        let config = config_with_max(131_072); // trigger ≈ 104k
        let reserved = ReservedContext::default();
        assert!(!session.needs_compaction(&config, reserved));

        // The provider saw a far larger prompt (ephemeral injections the
        // stored-message estimate can't see) — compaction must now fire.
        session.record_observed_input_tokens(120_000);
        assert!(session.needs_compaction(&config, reserved));
        assert!(session.needs_urgent_compaction(&config, reserved));

        // EVERY compaction path must clear the sample — it described the
        // pre-compaction prompt, and a surviving sample would re-trigger
        // compaction against a context that just shrank (grinding a healthy
        // session down through emergency keep-3 passes).
        session.compact_with_summary("condensed", 4, CompactionKind::Summarisation);
        assert_eq!(session.observed_input_tokens(), 0);
        assert!(!session.needs_compaction(&config, reserved));
        assert!(!session.needs_urgent_compaction(&config, reserved));
    }

    #[test]
    fn estimate_calibration_defaults_to_one_and_ewma_converges() {
        let mut session = session_with_message_tokens(10_000);
        assert_eq!(session.estimate_calibration(), 1.0);

        // Observed 2× the message estimate: EWMA (α = 0.3) steps 1.0 → 1.3 → 1.51.
        let reserved = ReservedContext::default();
        session.record_estimate_calibration(20_000, reserved);
        assert!((session.estimate_calibration() - 1.3).abs() < 1e-4);
        session.record_estimate_calibration(20_000, reserved);
        assert!((session.estimate_calibration() - 1.51).abs() < 1e-4);

        // Accurate observations decay the ratio back toward 1.0, never below.
        for _ in 0..50 {
            session.record_estimate_calibration(10_000, reserved);
        }
        assert!((session.estimate_calibration() - 1.0).abs() < 1e-3);
        assert!(session.estimate_calibration() >= 1.0);
    }

    #[test]
    fn estimate_calibration_samples_clamp_at_four() {
        let mut session = session_with_message_tokens(1_000);
        for _ in 0..100 {
            session.record_estimate_calibration(1_000_000, ReservedContext::default());
        }
        assert!(session.estimate_calibration() <= 4.0);
        assert!((session.estimate_calibration() - 4.0).abs() < 1e-3);
    }

    #[test]
    fn estimate_calibration_ratio_excludes_reserved_from_numerator() {
        let mut session = session_with_message_tokens(10_000);
        let reserved = ReservedContext {
            system_prompt_tokens: 10_000,
            tool_schema_tokens: 0,
            response_reservation_tokens: 0,
        };
        // Observed 25k minus the 10k reserved floor vs the 10k message
        // estimate → sample 1.5 → EWMA 1.0 + 0.3 × 0.5 = 1.15.
        session.record_estimate_calibration(25_000, reserved);
        assert!((session.estimate_calibration() - 1.15).abs() < 1e-4);
    }

    #[test]
    fn calibration_scales_message_term_but_not_reserved() {
        // 30k message estimate + 30k reserved = 60k raw — under the 65,536
        // trigger. Calibration 2.0 lifts only the message term: 90k fires the
        // trigger but stays under the 104,857 urgent threshold. Scaling the
        // reserved term too would read 120k and (wrongly) go urgent.
        let mut session = session_with_message_tokens(30_000);
        let config = config_with_max(131_072);
        let reserved = ReservedContext {
            system_prompt_tokens: 30_000,
            tool_schema_tokens: 0,
            response_reservation_tokens: 0,
        };
        assert!(!session.needs_compaction(&config, reserved));

        session.metadata = serde_json::json!({ "estimate_calibration": 2.0 });
        assert!(session.needs_compaction(&config, reserved));
        assert!(!session.needs_urgent_compaction(&config, reserved));
    }

    #[test]
    fn compaction_clears_observed_sample_but_not_calibration() {
        let mut session = session_with_message_tokens(10_000);
        session.record_observed_input_tokens(120_000);
        session.record_estimate_calibration(20_000, ReservedContext::default());
        let calibration = session.estimate_calibration();
        assert!(calibration > 1.0);

        session.compact_with_summary("condensed", 4, CompactionKind::Summarisation);
        assert_eq!(session.observed_input_tokens(), 0, "scalar sample cleared");
        assert_eq!(
            session.estimate_calibration(),
            calibration,
            "ratio survives compaction"
        );
    }

    #[test]
    fn a_pruning_keeps_the_observed_sample() {
        let mut session = session_with_message_tokens(10_000);
        session.record_observed_input_tokens(120_000);

        session.record_compaction(CompactionEvent {
            timestamp: Utc::now(),
            messages_compacted: 3,
            tokens_before: 10_000,
            tokens_after: 10_000,
            kind: CompactionKind::Pruning,
        });

        assert_eq!(session.observed_input_tokens(), 120_000);
    }

    #[test]
    fn estimate_calibration_survives_serde_round_trip() {
        let mut session = session_with_message_tokens(10_000);
        session.record_estimate_calibration(20_000, ReservedContext::default());
        let calibration = session.estimate_calibration();

        let json = serde_json::to_string(&session).unwrap();
        let restored: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.estimate_calibration(), calibration);
    }

    #[test]
    fn adopt_conversation_mode_copies_voice_keys_only() {
        let mut from = Session::new("ios", "u", "agent");
        from.metadata = serde_json::json!({
            "voice_mode": true,
            "tts_provider": "fishaudio",
            "seq": 7,
            "prev_session_id": "old",
            "rollover_pending": true,
        });
        let mut to = Session::new("ios", "u", "agent");
        to.set_seq(8);
        to.adopt_conversation_mode(&from);
        assert!(to.is_voice());
        assert_eq!(
            to.metadata.get("tts_provider").and_then(|v| v.as_str()),
            Some("fishaudio")
        );
        assert_eq!(to.seq(), 8, "rollover bookkeeping must not transfer");
        assert!(to.prev_session_id().is_none());
        assert!(!to.rollover_pending());
    }

    #[test]
    fn compaction_config_defaults_new_rollover_knobs() {
        let config = CompactionConfig::default();
        assert_eq!(config.stale_rollover_hours, 24);
        assert_eq!(config.voice_rollover_max_messages, 80);
        assert_eq!(config.chat_rollover_max_messages, 200);
        // Configs persisted before these knobs existed deserialize to the
        // same defaults.
        let legacy: CompactionConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.stale_rollover_hours, 24);
        assert_eq!(legacy.voice_rollover_max_messages, 80);
        assert_eq!(legacy.chat_rollover_max_messages, 200);
    }

    #[test]
    fn estimated_total_with_reserved_counts_system_prompt_once() {
        // The assembled prompt is stored as the session's System message AND
        // accounted for by ReservedContext::system_prompt_tokens. The threshold
        // must count it once, not twice (the regression that fired compaction
        // on every voice turn).
        let mut session = Session::new("c", "u", "agent");
        session.set_system_message(&"s".repeat(40_000 * 4)); // ~40K-token prompt
        session.add_user_message(&"u".repeat(100 * 4), None); // ~100-token message
        let reserved = ReservedContext {
            system_prompt_tokens: 40_000,
            tool_schema_tokens: 0,
            response_reservation_tokens: 0,
        };
        assert_eq!(session.non_system_message_tokens(), 100);
        // 100 (message) + 40_000 (reserved), not 40_100 + 40_000.
        assert_eq!(session.estimated_total_with_reserved(reserved), 40_100);
        // estimated_total_tokens still sums every message, prompt included.
        assert_eq!(session.estimated_total_tokens(), 40_100);
    }

    // ── Turn lifecycle records ──

    fn jsonl_store_as(dir: &tempfile::TempDir, instance: &str) -> JsonlSessionStore {
        JsonlSessionStore::new_as(dir.path(), instance).expect("failed to create store")
    }

    #[test]
    fn a_turn_record_outlives_a_whole_session_save_and_a_restart() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = {
            let store = jsonl_store_as(&dir, "hub-1");
            let mut session = store.get_or_create("slack:t:c:1", "u", "agent").unwrap();
            session.add_user_message("what plugs do I need?", None);
            store.save(&session).unwrap();
            let message_id = session.messages[0].id.clone();
            store
                .begin_turn(&session.id, Some(&message_id), &TurnPrincipal::default())
                .unwrap();

            store.save(&session).unwrap();
            assert!(
                store.interrupted_turns().unwrap().is_empty(),
                "our own record belongs to a live turn, never to a sweep"
            );
            session
        };

        // A fresh store with a new identity models the successor process.
        let successor = jsonl_store_as(&dir, "hub-2");
        let interrupted = successor.interrupted_turns().unwrap();
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].session_id, session.id);
        assert_eq!(
            interrupted[0].message_id.as_deref(),
            Some(session.messages[0].id.as_str())
        );

        successor
            .end_turn(&session.id, &interrupted[0].token)
            .unwrap();
        assert!(successor.interrupted_turns().unwrap().is_empty());
        successor
            .end_turn(&session.id, &interrupted[0].token)
            .expect("ending twice is a no-op");
    }

    #[test]
    fn a_deleted_session_leaves_no_turn_record_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = jsonl_store_as(&dir, "hub-1");
        let session = store.get_or_create("slack:t:c:1", "u", "agent").unwrap();
        store
            .begin_turn(&session.id, None, &TurnPrincipal::default())
            .unwrap();
        store.delete(&session.id).unwrap();
        assert!(jsonl_store_as(&dir, "hub-2")
            .interrupted_turns()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn in_memory_turn_records_round_trip() {
        let store = InMemorySessionStore::new_as("hub-1");
        let session = store.get_or_create("slack:t:c:1", "u", "agent").unwrap();

        let token = store
            .begin_turn(&session.id, Some("msg-1"), &TurnPrincipal::default())
            .unwrap();
        assert!(
            store.interrupted_turns().unwrap().is_empty(),
            "a live turn of ours is never a sweep candidate"
        );
        store.end_turn(&session.id, &token).unwrap();
        store
            .end_turn(&session.id, &token)
            .expect("ending twice is a no-op");
    }

    #[test]
    fn a_stale_token_never_clears_the_record_that_replaced_it() {
        // The stop-path race: turn A is cancelled, turn B opens its record, and
        // A's teardown lands afterwards. A's token no longer names the record,
        // so A clears nothing and B keeps its recovery state.
        let dir = tempfile::tempdir().expect("temp dir");
        let store = jsonl_store_as(&dir, "hub-1");
        let session = store.get_or_create("slack:t:c:1", "u", "agent").unwrap();

        let first = store
            .begin_turn(&session.id, Some("msg-a"), &TurnPrincipal::default())
            .unwrap();
        let second = store
            .begin_turn(&session.id, Some("msg-b"), &TurnPrincipal::default())
            .unwrap();
        assert_ne!(first, second, "each record gets its own token");

        store.end_turn(&session.id, &first).unwrap();
        let successor = jsonl_store_as(&dir, "hub-2");
        let interrupted = successor.interrupted_turns().unwrap();
        assert_eq!(interrupted.len(), 1, "the superseded turn cleared nothing");
        assert_eq!(interrupted[0].message_id.as_deref(), Some("msg-b"));
        assert_eq!(interrupted[0].token, second);

        store.end_turn(&session.id, &second).unwrap();
        assert!(successor.interrupted_turns().unwrap().is_empty());
    }

    #[test]
    fn an_in_memory_stale_token_clears_nothing() {
        let store = InMemorySessionStore::new_as("hub-1");
        let session = store.get_or_create("slack:t:c:1", "u", "agent").unwrap();
        let first = store
            .begin_turn(&session.id, Some("msg-a"), &TurnPrincipal::default())
            .unwrap();
        let second = store
            .begin_turn(&session.id, Some("msg-b"), &TurnPrincipal::default())
            .unwrap();

        store.end_turn(&session.id, &first).unwrap();
        let live = InMemorySessionStore::new_as("hub-2");
        assert!(
            live.interrupted_turns().unwrap().is_empty(),
            "separate store"
        );
        store.end_turn(&session.id, &second).unwrap();
        store
            .end_turn(&session.id, &second)
            .expect("ending twice is a no-op");
    }

    #[test]
    fn only_another_instances_record_is_an_interrupted_turn() {
        let turn = ActiveTurn::new("hub-1", Some("msg-1"), &TurnPrincipal::default());
        assert_eq!(turn.interrupted_by_other("s1", "hub-1"), None);
        let interrupted = turn
            .interrupted_by_other("s1", "hub-2")
            .expect("a foreign record is resumable");
        assert_eq!(interrupted.session_id, "s1");
        assert_eq!(interrupted.message_id.as_deref(), Some("msg-1"));
        assert_eq!(interrupted.started_at, Some(turn.started_at));
        assert_eq!(interrupted.token, turn.token);
    }

    #[test]
    fn a_turn_record_keeps_its_principal_and_a_legacy_record_reads_empty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = jsonl_store_as(&dir, "hub-1");
        let session = store.get_or_create("slack:t:c:1", "u", "agent").unwrap();
        let principal = TurnPrincipal {
            run_as: Some("6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11".into()),
            bill_to: Some("system:reports".into()),
        };
        store
            .begin_turn(&session.id, Some("msg-1"), &principal)
            .unwrap();
        let interrupted = jsonl_store_as(&dir, "hub-2").interrupted_turns().unwrap();
        assert_eq!(interrupted[0].principal, principal);
        assert_eq!(principal.billing(), Some("system:reports"));

        let legacy = serde_json::json!({
            "instance": "hub-1",
            "started_at": Utc::now(),
            "message_id": "msg-1",
            "token": "t",
        });
        let legacy: ActiveTurn = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.principal, TurnPrincipal::default());
        assert_eq!(TurnPrincipal::run_as(Some("u".into())).billing(), Some("u"));
    }

    #[test]
    fn work_is_readable_by_the_people_it_was_done_for_and_by_everyone_when_for_nobody() {
        let of = |run_as: Option<&str>, bill_to: Option<&str>| TurnPrincipal {
            run_as: run_as.map(str::to_string),
            bill_to: bill_to.map(str::to_string),
        };
        let hers = of(Some("ada"), Some("lead"));
        assert!(hers.readable_by(&of(Some("ada"), None)));
        assert!(hers.readable_by(&of(Some("lead"), None)));
        assert!(!hers.readable_by(&of(Some("bob"), None)));
        assert!(!hers.readable_by(&TurnPrincipal::default()));

        let billed = of(Some("system:heartbeat"), Some("ada"));
        assert!(
            billed.readable_by(&billed),
            "a lane billed to a person reads the work it started"
        );
        assert!(!billed.readable_by(&of(Some("system:heartbeat"), Some("bob"))));
        assert!(
            !of(Some("ada"), None).readable_by(&of(Some("bob"), Some("ada"))),
            "a person whose turn is billed to another does not read as them"
        );

        for nobody in [
            of(None, None),
            of(Some("system:heartbeat"), None),
            of(Some("agent:ops"), None),
        ] {
            assert!(nobody.readable_by(&of(Some("bob"), None)));
            assert!(nobody.readable_by(&TurnPrincipal::default()));
        }
    }
}

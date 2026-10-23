//! Slack-specific agent-callable tools.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use tracing::{info, warn};

use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::channels::{ChannelSurface, OutboundMessage, Reaction};
use workforce_common::emoji;

use crate::adapters::utils::{coerce_to_u64, decode_upload_content};

use super::surface::{SlackChannelSummary, SlackSurface, SlackUserSummary};

/// Tool that lets agents send messages to Slack channels.
pub struct SlackSendTool {
    surface: Arc<dyn ChannelSurface>,
}

impl SlackSendTool {
    pub fn new(surface: Arc<dyn ChannelSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackSendTool {
    fn name(&self) -> &str {
        "slack.send"
    }

    fn description(&self) -> &str {
        "Send a message to a Slack channel or thread."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Slack channel ID to send to"
                },
                "text": {
                    "type": "string",
                    "description": "Message text"
                },
                "thread_ts": {
                    "type": "string",
                    "description": "Thread timestamp for replying in a thread"
                }
            },
            "required": ["channel", "text"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let channel = input["channel"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'channel' parameter"))?;
        let text = input["text"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'text' parameter"))?;
        let thread_ts = input["thread_ts"].as_str();

        let message = OutboundMessage {
            channel: channel.to_string(),
            text: text.to_string(),
            thread_id: thread_ts.map(|s| s.to_string()),
            username: None,
            icon_url: None,
            blocks: None,
        };

        let duration_ms = start.elapsed().as_millis() as u64;

        match self.surface.send_message(&message).await {
            Ok(sent) => {
                info!(
                    channel = %channel,
                    message_id = %sent.message_id,
                    "Slack message sent"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "status": "ok",
                        "channel": sent.channel,
                        "message_id": sent.message_id,
                        "thread_id": sent.thread_id,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, channel = %channel, "Failed to send Slack message");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to send message"),
                    duration_ms,
                ))
            }
        }
    }
}

/// Tool that lets agents react to Slack messages with emoji.
pub struct SlackReactTool {
    surface: Arc<dyn ChannelSurface>,
}

impl SlackReactTool {
    pub fn new(surface: Arc<dyn ChannelSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackReactTool {
    fn name(&self) -> &str {
        "slack.react"
    }

    fn description(&self) -> &str {
        "Add an emoji reaction to a Slack message."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Channel ID where the message is"
                },
                "timestamp": {
                    "type": "string",
                    "description": "Message timestamp to react to"
                },
                "emoji": {
                    "type": "string",
                    "description": "Emoji name (e.g. 'thumbsup', ':eyes:', or Unicode)"
                }
            },
            "required": ["channel", "timestamp", "emoji"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let channel = input["channel"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'channel' parameter"))?;
        let timestamp = input["timestamp"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'timestamp' parameter"))?;
        let emoji_input = input["emoji"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'emoji' parameter"))?;

        let reaction = Reaction {
            channel: channel.to_string(),
            message_id: timestamp.to_string(),
            emoji: emoji_input.to_string(),
        };

        let duration_ms = start.elapsed().as_millis() as u64;

        match self.surface.add_reaction(&reaction).await {
            Ok(()) => {
                let emoji_name = emoji::to_slack_name(emoji_input);
                info!(emoji = %emoji_name, channel = %channel, "Reaction added");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "status": "ok",
                        "emoji": emoji_name,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Failed to add reaction");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to add reaction"),
                    duration_ms,
                ))
            }
        }
    }
}

/// Tool that lets agents read recent messages from a Slack channel.
pub struct SlackReadTool {
    surface: Arc<dyn ChannelSurface>,
}

impl SlackReadTool {
    pub fn new(surface: Arc<dyn ChannelSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackReadTool {
    fn name(&self) -> &str {
        "slack.read"
    }

    fn description(&self) -> &str {
        "Read messages from a Slack channel, newest first. Without `oldest`/`latest` this returns \
         one page of recent messages (max 100). Set `oldest` (a Slack ts) to walk back through \
         history: the call pages until the range is exhausted or the per-call budget (20 pages of \
         `limit`) runs out — then `truncated` is true and `next_latest` is the resume cursor; call \
         again with the same `oldest` and `latest: next_latest` to continue. Thread replies are \
         not included — fetch them per thread with slack.get_message_replies."
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
                    "description": "Messages per page (integer, default: 20, max: 100). With `oldest`/`latest` set, a call returns up to 20 pages of this size.",
                    "minimum": 1,
                    "maximum": 100
                },
                "oldest": {
                    "type": "string",
                    "description": "Slack timestamp (e.g. \"1747033200.000000\"); only messages strictly newer than this are returned, paging as needed."
                },
                "latest": {
                    "type": "string",
                    "description": "Slack timestamp; only messages strictly older than this are returned. Pass a previous call's `next_latest` to resume a truncated walk."
                }
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

        let duration_ms = start.elapsed().as_millis() as u64;

        if oldest.is_some() || latest.is_some() {
            return match self
                .surface
                .read_history(channel, limit, oldest, latest)
                .await
            {
                Ok(page) => {
                    let count = page.messages.len();
                    info!(channel = %channel, count = count, truncated = page.truncated, "Read Slack history");
                    let next_latest = page
                        .truncated
                        .then(|| page.messages.last().map(|m| m.timestamp.clone()))
                        .flatten();
                    Ok(ToolResult::success(
                        serde_json::json!({
                            "channel": channel,
                            "count": count,
                            "messages": page.messages,
                            "truncated": page.truncated,
                            "next_latest": next_latest,
                        }),
                        duration_ms,
                    ))
                }
                Err(e) => {
                    warn!(error = %e, channel = %channel, "Failed to read history");
                    Ok(ToolResult::from_report(
                        &e.wrap_err("Failed to read history"),
                        duration_ms,
                    ))
                }
            };
        }

        match self.surface.read_messages(channel, limit, None).await {
            Ok(messages) => {
                let count = messages.len();
                info!(channel = %channel, count = count, "Read Slack messages");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "channel": channel,
                        "count": count,
                        "messages": messages,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, channel = %channel, "Failed to read messages");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to read messages"),
                    duration_ms,
                ))
            }
        }
    }
}

/// Tool that lets agents look up Slack channel information.
pub struct SlackChannelInfoTool {
    surface: Arc<super::SlackSurface>,
}

impl SlackChannelInfoTool {
    pub fn new(surface: Arc<super::SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackChannelInfoTool {
    fn name(&self) -> &str {
        "slack.channel_info"
    }

    fn description(&self) -> &str {
        "Look up information about a Slack channel, including its human-readable name"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Slack channel ID (e.g., C12345, D67890)"
                }
            },
            "required": ["channel"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let channel = input["channel"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'channel' parameter"))?;

        let duration_ms = start.elapsed().as_millis() as u64;

        match self.surface.resolve_channel_name(channel).await {
            Ok(name) => Ok(ToolResult::success(
                serde_json::json!({
                    "channel_id": channel,
                    "display_name": name,
                }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to resolve channel"),
                duration_ms,
            )),
        }
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

fn parse_input<T: serde::de::DeserializeOwned>(
    input: serde_json::Value,
    start: Instant,
) -> std::result::Result<T, ToolResult> {
    serde_json::from_value::<T>(input)
        .map_err(|e| ToolResult::failure(format!("invalid input: {e}"), elapsed_ms(start)))
}

fn to_tool_result<T: serde::Serialize>(result: Result<T>, start: Instant) -> ToolResult {
    match result.and_then(|v| serde_json::to_value(v).map_err(eyre::Report::from)) {
        Ok(json) => ToolResult::success(json, elapsed_ms(start)),
        Err(e) => ToolResult::from_report(&e, elapsed_ms(start)),
    }
}

fn match_channel(c: &SlackChannelSummary, needle: Option<&str>) -> bool {
    let Some(n) = needle else {
        return true;
    };
    let n = n.to_lowercase();
    c.name.to_lowercase().contains(&n)
        || c.topic
            .as_deref()
            .map(|t| t.to_lowercase().contains(&n))
            .unwrap_or(false)
        || c.purpose
            .as_deref()
            .map(|p| p.to_lowercase().contains(&n))
            .unwrap_or(false)
}

fn user_haystack_contains(u: &SlackUserSummary, needle: &str) -> bool {
    let n = needle.to_lowercase();
    u.name.to_lowercase().contains(&n)
        || u.real_name
            .as_deref()
            .map(|r| r.to_lowercase().contains(&n))
            .unwrap_or(false)
        || u.display_name
            .as_deref()
            .map(|d| d.to_lowercase().contains(&n))
            .unwrap_or(false)
        || u.email
            .as_deref()
            .map(|e| e.to_lowercase().contains(&n))
            .unwrap_or(false)
}

fn email_contains(u: &SlackUserSummary, needle: &str) -> bool {
    let n = needle.to_lowercase();
    u.email
        .as_deref()
        .map(|e| e.to_lowercase().contains(&n))
        .unwrap_or(false)
}

const DEFAULT_LIST_LIMIT: u32 = 200;
const DEFAULT_CHANNEL_TYPES: &str = "public_channel,private_channel";
const MAX_FIND_PAGES: u32 = 25;
const MAX_FIND_MATCHES: usize = 50;

fn nonempty(s: Option<&String>) -> Option<&str> {
    s.map(String::as_str).filter(|v| !v.is_empty())
}

fn match_user(u: &SlackUserSummary, p: &ListUsersInput) -> bool {
    if !p.include_deleted && u.is_deleted {
        return false;
    }
    if !p.include_bots && u.is_bot {
        return false;
    }
    if let Some(f) = nonempty(p.filter.as_ref()) {
        if !user_haystack_contains(u, f) {
            return false;
        }
    }
    if let Some(e) = nonempty(p.email.as_ref()) {
        if !email_contains(u, e) {
            return false;
        }
    }
    true
}

async fn find_users(
    surface: &SlackSurface,
    params: &ListUsersInput,
) -> eyre::Result<serde_json::Value> {
    let mut users: Vec<SlackUserSummary> = Vec::new();
    let mut cursor = params.cursor.clone();
    let mut pages = 0u32;
    let next_cursor: Option<String> = loop {
        let page = surface.list_users(params.limit, cursor.as_deref()).await?;
        pages += 1;
        users.extend(page.users.into_iter().filter(|u| match_user(u, params)));
        if users.len() >= MAX_FIND_MATCHES {
            break page.next_cursor;
        }
        match page.next_cursor {
            None => break None,
            Some(c) if pages >= MAX_FIND_PAGES => break Some(c),
            Some(c) => cursor = Some(c),
        }
    };
    let truncated = next_cursor.is_some();
    let count = users.len();
    Ok(serde_json::json!({
        "users": users,
        "next_cursor": next_cursor,
        "count": count,
        "truncated": truncated,
    }))
}

#[derive(Deserialize)]
struct ListChannelsInput {
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    types: Option<String>,
    #[serde(default = "default_true")]
    exclude_archived: bool,
    #[serde(default = "default_list_limit")]
    limit: u32,
    #[serde(default)]
    cursor: Option<String>,
}

fn default_true() -> bool {
    true
}
fn default_list_limit() -> u32 {
    DEFAULT_LIST_LIMIT
}

pub struct SlackListChannelsTool {
    surface: Arc<SlackSurface>,
}

impl SlackListChannelsTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackListChannelsTool {
    fn name(&self) -> &str {
        "slack.list_channels"
    }
    fn description(&self) -> &str {
        "List channels in the Slack workspace. Returns id, name, topic, purpose, member count, and archive/private/is_member flags. Use this to discover channel IDs before reading or sending. Optional `filter` is a case-insensitive substring matched against name/topic/purpose. `types` accepts a Slack-native CSV (default: public_channel,private_channel)."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {"type": "string", "description": "Case-insensitive substring against name/topic/purpose."},
                "types": {"type": "string", "description": "Slack-native CSV: public_channel,private_channel,mpim,im. Default: public_channel,private_channel."},
                "exclude_archived": {"type": "boolean", "default": true},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": DEFAULT_LIST_LIMIT},
                "cursor": {"type": "string", "description": "Opaque cursor from a previous call's next_cursor."}
            }
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: ListChannelsInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let types = params
            .types
            .clone()
            .unwrap_or_else(|| DEFAULT_CHANNEL_TYPES.to_string());
        let page = self
            .surface
            .list_channels(
                &types,
                params.exclude_archived,
                params.limit,
                params.cursor.as_deref(),
            )
            .await;
        let outcome = page.map(|p| {
            let filter = params.filter.as_deref();
            let channels: Vec<SlackChannelSummary> = p
                .channels
                .into_iter()
                .filter(|c| match_channel(c, filter))
                .collect();
            let count = channels.len();
            serde_json::json!({
                "channels": channels,
                "next_cursor": p.next_cursor,
                "count": count,
            })
        });
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct ListUsersInput {
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    include_deleted: bool,
    #[serde(default)]
    include_bots: bool,
    #[serde(default = "default_list_limit")]
    limit: u32,
    #[serde(default)]
    cursor: Option<String>,
}

pub struct SlackListUsersTool {
    surface: Arc<SlackSurface>,
}

impl SlackListUsersTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackListUsersTool {
    fn name(&self) -> &str {
        "slack.list_users"
    }
    fn description(&self) -> &str {
        "Find users in the Slack workspace by name or email. Returns id, name, real name, email (when the bot has users:read.email scope), bot/deleted flags. \
         When `filter` (substring match on name/real_name/display_name/email) or `email` (substring on email) is set, this walks pagination on your behalf — up to 25 pages / 50 matches — so a one-shot call is enough. \
         Without either, returns one raw page and you must follow `next_cursor` yourself. \
         If the response has `truncated: true`, more matches may exist beyond the budget; narrow the filter or resume with `cursor: next_cursor`. \
         Bots and deleted users excluded by default — set include_bots/include_deleted to include."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {"type": "string", "description": "Case-insensitive substring matched against name/real_name/display_name/email. Triggers full-workspace walk."},
                "email": {"type": "string", "description": "Case-insensitive substring matched against email only. Triggers full-workspace walk. For exact email lookup, use slack.lookup_user_by_email instead."},
                "include_deleted": {"type": "boolean", "default": false},
                "include_bots": {"type": "boolean", "default": false},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": DEFAULT_LIST_LIMIT},
                "cursor": {"type": "string", "description": "Opaque cursor — for raw pagination, or to resume a truncated filter walk."}
            }
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: ListUsersInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let walk =
            nonempty(params.filter.as_ref()).is_some() || nonempty(params.email.as_ref()).is_some();
        let outcome = if walk {
            find_users(&self.surface, &params).await
        } else {
            self.surface
                .list_users(params.limit, params.cursor.as_deref())
                .await
                .map(|p| {
                    let users: Vec<SlackUserSummary> = p
                        .users
                        .into_iter()
                        .filter(|u| match_user(u, &params))
                        .collect();
                    let count = users.len();
                    let truncated = p.next_cursor.is_some();
                    serde_json::json!({
                        "users": users,
                        "next_cursor": p.next_cursor,
                        "count": count,
                        "truncated": truncated,
                    })
                })
        };
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct LookupUserByEmailInput {
    email: String,
}

pub struct SlackLookupUserByEmailTool {
    surface: Arc<SlackSurface>,
}

impl SlackLookupUserByEmailTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackLookupUserByEmailTool {
    fn name(&self) -> &str {
        "slack.lookup_user_by_email"
    }
    fn description(&self) -> &str {
        "Look up a single Slack user by exact email address. Cheaper than draining slack.list_users when you already have the email. Requires the users:read.email scope on the bot token."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"email": {"type": "string"}},
            "required": ["email"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: LookupUserByEmailInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .lookup_user_by_email(&params.email)
            .await
            .map(|user| serde_json::json!({ "user": user }));
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct ListChannelMembersInput {
    channel: String,
    #[serde(default = "default_list_limit")]
    limit: u32,
    #[serde(default)]
    cursor: Option<String>,
}

pub struct SlackListChannelMembersTool {
    surface: Arc<SlackSurface>,
}

impl SlackListChannelMembersTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackListChannelMembersTool {
    fn name(&self) -> &str {
        "slack.list_channel_members"
    }
    fn description(&self) -> &str {
        "List the user IDs of all members in a Slack channel. Paginated; pass cursor for next page. Returns just IDs — call slack.user_info or slack.list_users to resolve names."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": DEFAULT_LIST_LIMIT},
                "cursor": {"type": "string"}
            },
            "required": ["channel"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: ListChannelMembersInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .list_channel_members(&params.channel, params.limit, params.cursor.as_deref())
            .await
            .map(|p| {
                let count = p.members.len();
                serde_json::json!({
                    "members": p.members,
                    "next_cursor": p.next_cursor,
                    "count": count,
                })
            });
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct ListUserConversationsInput {
    user: String,
    #[serde(default)]
    types: Option<String>,
    #[serde(default = "default_true")]
    exclude_archived: bool,
    #[serde(default = "default_list_limit")]
    limit: u32,
    #[serde(default)]
    cursor: Option<String>,
}

pub struct SlackListUserConversationsTool {
    surface: Arc<SlackSurface>,
}

impl SlackListUserConversationsTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackListUserConversationsTool {
    fn name(&self) -> &str {
        "slack.list_user_conversations"
    }
    fn description(&self) -> &str {
        "List channels a given user is a member of. Useful for 'what channels is X in?'. Paginated."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "user": {"type": "string", "description": "User ID."},
                "types": {"type": "string", "description": "CSV: public_channel,private_channel,mpim,im. Default: public_channel,private_channel."},
                "exclude_archived": {"type": "boolean", "default": true},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": DEFAULT_LIST_LIMIT},
                "cursor": {"type": "string"}
            },
            "required": ["user"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: ListUserConversationsInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let types = params
            .types
            .unwrap_or_else(|| DEFAULT_CHANNEL_TYPES.to_string());
        let outcome = self
            .surface
            .list_user_conversations(
                &params.user,
                &types,
                params.exclude_archived,
                params.limit,
                params.cursor.as_deref(),
            )
            .await
            .map(|p| {
                let count = p.channels.len();
                serde_json::json!({
                    "channels": p.channels,
                    "next_cursor": p.next_cursor,
                    "count": count,
                })
            });
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct SearchMessagesInput {
    query: String,
    #[serde(default = "default_search_count")]
    count: u32,
    #[serde(default)]
    cursor: Option<String>,
}

fn default_search_count() -> u32 {
    20
}

pub struct SlackSearchMessagesTool {
    surface: Arc<SlackSurface>,
}

impl SlackSearchMessagesTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackSearchMessagesTool {
    fn name(&self) -> &str {
        "slack.search_messages"
    }
    fn description(&self) -> &str {
        "Search messages across the Slack workspace using Slack's query syntax (e.g. 'incident after:2026-05-01 in:#ops from:@allan'). Requires a per-user xoxp-… token with the search:read scope and a paid Slack plan. Each caller authenticates with their OWN Slack identity — register via slack.user_token_set workspace_id=… user_token=… before searching."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "count": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 20},
                "cursor": {"type": "string"}
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: SearchMessagesInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .search_messages(&params.query, params.count, params.cursor.as_deref())
            .await
            .map(|p| {
                let count = p.matches.len();
                serde_json::json!({
                    "matches": p.matches,
                    "total": p.total,
                    "next_cursor": p.next_cursor,
                    "count": count,
                })
            });
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct GetMessageRepliesInput {
    channel: String,
    ts: String,
    #[serde(default = "default_list_limit")]
    limit: u32,
    #[serde(default)]
    cursor: Option<String>,
}

pub struct SlackGetMessageRepliesTool {
    surface: Arc<SlackSurface>,
}

impl SlackGetMessageRepliesTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackGetMessageRepliesTool {
    fn name(&self) -> &str {
        "slack.get_message_replies"
    }
    fn description(&self) -> &str {
        "Fetch the replies in a Slack thread. `ts` is the parent message timestamp. Returns parent + replies in order; paginated for long threads."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {"type": "string"},
                "ts": {"type": "string", "description": "Parent message timestamp."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": DEFAULT_LIST_LIMIT},
                "cursor": {"type": "string"}
            },
            "required": ["channel", "ts"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: GetMessageRepliesInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .get_message_replies(
                &params.channel,
                &params.ts,
                params.limit,
                params.cursor.as_deref(),
            )
            .await
            .map(|p| {
                let count = p.replies.len();
                serde_json::json!({
                    "replies": p.replies,
                    "has_more": p.has_more,
                    "next_cursor": p.next_cursor,
                    "count": count,
                })
            });
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct UserInfoInput {
    user: String,
}

pub struct SlackUserInfoTool {
    surface: Arc<SlackSurface>,
}

impl SlackUserInfoTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackUserInfoTool {
    fn name(&self) -> &str {
        "slack.user_info"
    }
    fn description(&self) -> &str {
        "Fetch full details for a single user ID. Mirrors slack.channel_info but for users."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"user": {"type": "string"}},
            "required": ["user"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: UserInfoInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .get_user_info(&params.user)
            .await
            .map(|user| serde_json::json!({ "user": user }));
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct InviteToChannelInput {
    channel: String,
    users: Vec<String>,
}

pub struct SlackInviteToChannelTool {
    surface: Arc<SlackSurface>,
}

impl SlackInviteToChannelTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackInviteToChannelTool {
    fn name(&self) -> &str {
        "slack.invite_to_channel"
    }
    fn description(&self) -> &str {
        "Invite one or more users to a Slack channel (up to 1000 per call). `users` is an array of user IDs."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {"type": "string"},
                "users": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 1000}
            },
            "required": ["channel", "users"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: InviteToChannelInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        if params.users.is_empty() {
            return Ok(ToolResult::failure(
                "invite_to_channel: `users` must be non-empty".to_string(),
                elapsed_ms(start),
            ));
        }
        let outcome = self
            .surface
            .invite_to_channel(&params.channel, &params.users)
            .await
            .map(|r| {
                let invited_count = r.invited.len();
                serde_json::json!({
                    "channel_id": r.channel_id,
                    "invited": r.invited,
                    "invited_count": invited_count,
                    "errors": r.errors,
                })
            });
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct UpdateMessageInput {
    channel: String,
    ts: String,
    text: String,
}

pub struct SlackUpdateMessageTool {
    surface: Arc<SlackSurface>,
}

impl SlackUpdateMessageTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackUpdateMessageTool {
    fn name(&self) -> &str {
        "slack.update_message"
    }
    fn description(&self) -> &str {
        "Edit a Slack message the bot previously sent. Bots can only edit their own messages. `ts` is the message timestamp from the original send."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {"type": "string"},
                "ts": {"type": "string"},
                "text": {"type": "string"}
            },
            "required": ["channel", "ts", "text"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: UpdateMessageInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .update_message_chat(&params.channel, &params.ts, &params.text)
            .await
            .map(|_| serde_json::json!({"ok": true, "channel": params.channel, "ts": params.ts}));
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct DeleteMessageInput {
    channel: String,
    ts: String,
}

pub struct SlackDeleteMessageTool {
    surface: Arc<SlackSurface>,
}

impl SlackDeleteMessageTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackDeleteMessageTool {
    fn name(&self) -> &str {
        "slack.delete_message"
    }
    fn description(&self) -> &str {
        "Delete a Slack message the bot previously sent. Permanent — Slack does not soft-delete via this API. Bots can only delete their own messages."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"channel": {"type": "string"}, "ts": {"type": "string"}},
            "required": ["channel", "ts"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: DeleteMessageInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .delete_message_chat(&params.channel, &params.ts)
            .await
            .map(|_| serde_json::json!({"ok": true, "channel": params.channel, "ts": params.ts}));
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct PinInput {
    channel: String,
    ts: String,
}

pub struct SlackPinMessageTool {
    surface: Arc<SlackSurface>,
}

impl SlackPinMessageTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackPinMessageTool {
    fn name(&self) -> &str {
        "slack.pin_message"
    }
    fn description(&self) -> &str {
        "Pin a message in a Slack channel. Channels have a 100-pin limit; unpin something first if Slack rejects."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"channel": {"type": "string"}, "ts": {"type": "string"}},
            "required": ["channel", "ts"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: PinInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .pin_message(&params.channel, &params.ts)
            .await
            .map(|_| serde_json::json!({"ok": true, "channel": params.channel, "ts": params.ts}));
        Ok(to_tool_result(outcome, start))
    }
}

pub struct SlackUnpinMessageTool {
    surface: Arc<SlackSurface>,
}

impl SlackUnpinMessageTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackUnpinMessageTool {
    fn name(&self) -> &str {
        "slack.unpin_message"
    }
    fn description(&self) -> &str {
        "Unpin a previously pinned message."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"channel": {"type": "string"}, "ts": {"type": "string"}},
            "required": ["channel", "ts"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: PinInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .unpin_message(&params.channel, &params.ts)
            .await
            .map(|_| serde_json::json!({"ok": true, "channel": params.channel, "ts": params.ts}));
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct CreateChannelInput {
    name: String,
    #[serde(default)]
    is_private: bool,
}

pub struct SlackCreateChannelTool {
    surface: Arc<SlackSurface>,
}

impl SlackCreateChannelTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackCreateChannelTool {
    fn name(&self) -> &str {
        "slack.create_channel"
    }
    fn description(&self) -> &str {
        "Create a Slack channel. `name` must be lowercase, ≤80 chars, letters/numbers/hyphens/underscores only. Returns the new channel's ID and resolved name."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "is_private": {"type": "boolean", "default": false}
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: CreateChannelInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .create_channel(&params.name, params.is_private)
            .await
            .map(|c| serde_json::json!({ "channel": c }));
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct ChannelOnlyInput {
    channel: String,
}

pub struct SlackArchiveChannelTool {
    surface: Arc<SlackSurface>,
}

impl SlackArchiveChannelTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackArchiveChannelTool {
    fn name(&self) -> &str {
        "slack.archive_channel"
    }
    fn description(&self) -> &str {
        "Archive a Slack channel (soft-delete; reversible via slack.unarchive_channel). #general cannot be archived."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"channel": {"type": "string"}},
            "required": ["channel"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: ChannelOnlyInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .archive_channel(&params.channel)
            .await
            .map(|_| serde_json::json!({"ok": true, "channel": params.channel}));
        Ok(to_tool_result(outcome, start))
    }
}

pub struct SlackUnarchiveChannelTool {
    surface: Arc<SlackSurface>,
}

impl SlackUnarchiveChannelTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackUnarchiveChannelTool {
    fn name(&self) -> &str {
        "slack.unarchive_channel"
    }
    fn description(&self) -> &str {
        "Unarchive a previously archived Slack channel."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"channel": {"type": "string"}},
            "required": ["channel"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: ChannelOnlyInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .unarchive_channel(&params.channel)
            .await
            .map(|_| serde_json::json!({"ok": true, "channel": params.channel}));
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct SetChannelMetadataInput {
    channel: String,
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    purpose: Option<String>,
}

pub struct SlackSetChannelMetadataTool {
    surface: Arc<SlackSurface>,
}

impl SlackSetChannelMetadataTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackSetChannelMetadataTool {
    fn name(&self) -> &str {
        "slack.set_channel_metadata"
    }
    fn description(&self) -> &str {
        "Set the topic and/or purpose of a Slack channel. At least one of `topic` or `purpose` must be provided. The two API calls run serially: if the topic call fails, the purpose call is not attempted."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {"type": "string"},
                "topic": {"type": "string"},
                "purpose": {"type": "string"}
            },
            "required": ["channel"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: SetChannelMetadataInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        if params.topic.is_none() && params.purpose.is_none() {
            return Ok(ToolResult::failure(
                "set_channel_metadata: at least one of `topic` or `purpose` must be provided"
                    .to_string(),
                elapsed_ms(start),
            ));
        }
        if let Some(topic) = params.topic.as_deref() {
            if let Err(e) = self.surface.set_channel_topic(&params.channel, topic).await {
                return Ok(ToolResult::from_report(&e, elapsed_ms(start)));
            }
        }
        if let Some(purpose) = params.purpose.as_deref() {
            if let Err(e) = self
                .surface
                .set_channel_purpose(&params.channel, purpose)
                .await
            {
                return Ok(ToolResult::from_report(&e, elapsed_ms(start)));
            }
        }
        Ok(ToolResult::success(
            serde_json::json!({
                "ok": true,
                "channel": params.channel,
                "topic_set": params.topic.is_some(),
                "purpose_set": params.purpose.is_some(),
            }),
            elapsed_ms(start),
        ))
    }
}

#[derive(Deserialize)]
struct RenameChannelInput {
    channel: String,
    name: String,
}

pub struct SlackRenameChannelTool {
    surface: Arc<SlackSurface>,
}

impl SlackRenameChannelTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackRenameChannelTool {
    fn name(&self) -> &str {
        "slack.rename_channel"
    }
    fn description(&self) -> &str {
        "Rename a Slack channel. Same name constraints as slack.create_channel."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"channel": {"type": "string"}, "name": {"type": "string"}},
            "required": ["channel", "name"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: RenameChannelInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .rename_channel(&params.channel, &params.name)
            .await
            .map(|c| serde_json::json!({ "channel": c }));
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct KickFromChannelInput {
    channel: String,
    user: String,
}

pub struct SlackKickFromChannelTool {
    surface: Arc<SlackSurface>,
}

impl SlackKickFromChannelTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackKickFromChannelTool {
    fn name(&self) -> &str {
        "slack.kick_from_channel"
    }
    fn description(&self) -> &str {
        "Remove a user from a Slack channel. Destructive: the user receives a Slack notification. Use only when you've confirmed the intent."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"channel": {"type": "string"}, "user": {"type": "string"}},
            "required": ["channel", "user"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: KickFromChannelInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .kick_from_channel(&params.channel, &params.user)
            .await
            .map(
                |_| serde_json::json!({"ok": true, "channel": params.channel, "user": params.user}),
            );
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct UploadFileInput {
    filename: String,
    #[serde(default)]
    content_text: Option<String>,
    #[serde(default)]
    content_base64: Option<String>,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    initial_comment: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
}

const MAX_UPLOAD_BYTES: usize = 1_000_000_000;

pub struct SlackUploadFileTool {
    surface: Arc<SlackSurface>,
}

impl SlackUploadFileTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackUploadFileTool {
    fn name(&self) -> &str {
        "slack.upload_file"
    }
    fn description(&self) -> &str {
        "Upload a file to Slack. Provide exactly one of `content_text` (UTF-8 text) or `content_base64` (base64-encoded bytes). Optionally share to a channel via `channel_id` with an `initial_comment` (and `thread_ts` for thread replies). Hard limit 1GB."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "filename": {"type": "string"},
                "content_text": {"type": "string", "description": "UTF-8 file body. Mutually exclusive with content_base64."},
                "content_base64": {"type": "string", "description": "Base64-encoded file body. Mutually exclusive with content_text."},
                "channel_id": {"type": "string"},
                "initial_comment": {"type": "string"},
                "thread_ts": {"type": "string"}
            },
            "required": ["filename"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: UploadFileInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let bytes = match decode_upload_content(
            params.content_text.as_deref(),
            params.content_base64.as_deref(),
        ) {
            Ok(bytes) => bytes,
            Err(msg) => {
                warn!(filename = %params.filename, error = %msg, "upload_file content rejected");
                return Ok(ToolResult::failure(msg, elapsed_ms(start)));
            }
        };
        if bytes.len() > MAX_UPLOAD_BYTES {
            return Ok(ToolResult::failure(
                format!(
                    "upload_file: file size {} bytes exceeds Slack's 1GB limit",
                    bytes.len()
                ),
                elapsed_ms(start),
            ));
        }
        let outcome = self
            .surface
            .upload_file_v2(
                &params.filename,
                &bytes,
                params.channel_id.as_deref(),
                params.initial_comment.as_deref(),
                params.thread_ts.as_deref(),
            )
            .await
            .map(|r| serde_json::json!({ "file": r }));
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct ListFilesInput {
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    types: Option<String>,
    #[serde(default = "default_list_limit")]
    limit: u32,
    #[serde(default)]
    cursor: Option<String>,
}

pub struct SlackListFilesTool {
    surface: Arc<SlackSurface>,
}

impl SlackListFilesTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackListFilesTool {
    fn name(&self) -> &str {
        "slack.list_files"
    }
    fn description(&self) -> &str {
        "List files visible to the bot, optionally filtered by channel, user, or file type (Slack CSV like 'images,pdfs')."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {"type": "string"},
                "user": {"type": "string"},
                "types": {"type": "string", "description": "CSV: images, pdfs, snippets, etc."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": DEFAULT_LIST_LIMIT},
                "cursor": {"type": "string"}
            }
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: ListFilesInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .list_files_for_agent(
                params.channel.as_deref(),
                params.user.as_deref(),
                params.types.as_deref(),
                params.limit,
                params.cursor.as_deref(),
            )
            .await
            .map(|p| {
                let count = p.files.len();
                serde_json::json!({
                    "files": p.files,
                    "next_cursor": p.next_cursor,
                    "count": count,
                })
            });
        Ok(to_tool_result(outcome, start))
    }
}

#[derive(Deserialize)]
struct DeleteFileInput {
    file: String,
}

pub struct SlackDeleteFileTool {
    surface: Arc<SlackSurface>,
}

impl SlackDeleteFileTool {
    pub fn new(surface: Arc<SlackSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Tool for SlackDeleteFileTool {
    fn name(&self) -> &str {
        "slack.delete_file"
    }
    fn description(&self) -> &str {
        "Delete a Slack file by ID. The bot must be the uploader."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"file": {"type": "string"}},
            "required": ["file"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: DeleteFileInput = match parse_input(input, start) {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        let outcome = self
            .surface
            .delete_file(&params.file)
            .await
            .map(|_| serde_json::json!({"ok": true, "file": params.file}));
        Ok(to_tool_result(outcome, start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use workforce_common::channels::{
        ChannelCapabilities, ChannelMessage, HistoryPage, SentMessage,
    };

    /// Mock surface that records calls and returns predictable results.
    struct MockSurface {
        send_count: AtomicUsize,
        react_count: AtomicUsize,
        read_count: AtomicUsize,
        history_count: AtomicUsize,
    }

    impl MockSurface {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                send_count: AtomicUsize::new(0),
                react_count: AtomicUsize::new(0),
                read_count: AtomicUsize::new(0),
                history_count: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl ChannelSurface for MockSurface {
        fn platform(&self) -> &str {
            "mock"
        }

        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities {
                reactions: true,
                reaction_removal: true,
                threads: true,
                rich_text: true,
                send_message: true,
                read_messages: true,
                edit_message: true,
                delete_message: true,
                read_thread: true,
                pins: false,
                member_info: false,
                emoji_list: false,
                file_upload: false,
                channel_info: false,
            }
        }

        async fn add_reaction(&self, _reaction: &Reaction) -> Result<()> {
            self.react_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn remove_reaction(&self, _reaction: &Reaction) -> Result<()> {
            Ok(())
        }

        async fn send_message(&self, message: &OutboundMessage) -> Result<SentMessage> {
            self.send_count.fetch_add(1, Ordering::SeqCst);
            Ok(SentMessage {
                channel: message.channel.clone(),
                message_id: "1234567890.123456".to_string(),
                thread_id: message.thread_id.clone(),
                platform: "mock".to_string(),
            })
        }

        async fn read_messages(
            &self,
            channel: &str,
            _limit: usize,
            _oldest: Option<&str>,
        ) -> Result<Vec<ChannelMessage>> {
            self.read_count.fetch_add(1, Ordering::SeqCst);
            Ok(vec![ChannelMessage {
                channel: channel.to_string(),
                message_id: "100.001".to_string(),
                user: "U_USER".to_string(),
                user_name: None,
                channel_name: None,
                text: "hello world".to_string(),
                timestamp: "100.001".to_string(),
                thread_id: None,
                files: vec![],
                bot_id: None,
            }])
        }

        async fn read_history(
            &self,
            channel: &str,
            _limit: usize,
            _oldest: Option<&str>,
            _latest: Option<&str>,
        ) -> Result<HistoryPage> {
            self.history_count.fetch_add(1, Ordering::SeqCst);
            let msg = |ts: &str| ChannelMessage {
                channel: channel.to_string(),
                message_id: ts.to_string(),
                user: "U_USER".to_string(),
                user_name: None,
                channel_name: None,
                text: format!("msg {ts}"),
                timestamp: ts.to_string(),
                thread_id: None,
                files: vec![],
                bot_id: None,
            };
            Ok(HistoryPage {
                messages: vec![msg("100.002"), msg("100.001")],
                truncated: true,
            })
        }
    }

    // ── SlackSendTool tests ─────────────────────────────────────────

    #[test]
    fn send_tool_name_and_schema() {
        let surface = MockSurface::new();
        let tool = SlackSendTool::new(surface);
        assert_eq!(tool.name(), "slack.send");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["channel", "text"]));
    }

    #[tokio::test]
    async fn send_tool_sends_message() {
        let surface = MockSurface::new();
        let tool = SlackSendTool::new(Arc::clone(&surface) as Arc<dyn ChannelSurface>);
        let input = serde_json::json!({
            "channel": "C123",
            "text": "hello"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["status"], "ok");
        assert_eq!(result.output["channel"], "C123");
        assert_eq!(surface.send_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn send_tool_with_thread() {
        let surface = MockSurface::new();
        let tool = SlackSendTool::new(Arc::clone(&surface) as Arc<dyn ChannelSurface>);
        let input = serde_json::json!({
            "channel": "C123",
            "text": "reply",
            "thread_ts": "100.001"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["thread_id"], "100.001");
    }

    #[tokio::test]
    async fn send_tool_missing_channel_fails() {
        let surface = MockSurface::new();
        let tool = SlackSendTool::new(surface);
        let input = serde_json::json!({"text": "hello"});
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn send_tool_missing_text_fails() {
        let surface = MockSurface::new();
        let tool = SlackSendTool::new(surface);
        let input = serde_json::json!({"channel": "C123"});
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    // ── SlackReactTool tests ────────────────────────────────────────

    #[test]
    fn react_tool_name_and_schema() {
        let surface = MockSurface::new();
        let tool = SlackReactTool::new(surface);
        assert_eq!(tool.name(), "slack.react");
        let schema = tool.input_schema();
        assert_eq!(
            schema["required"],
            serde_json::json!(["channel", "timestamp", "emoji"])
        );
    }

    #[tokio::test]
    async fn react_tool_adds_reaction() {
        let surface = MockSurface::new();
        let tool = SlackReactTool::new(Arc::clone(&surface) as Arc<dyn ChannelSurface>);
        let input = serde_json::json!({
            "channel": "C123",
            "timestamp": "100.001",
            "emoji": "thumbsup"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["status"], "ok");
        assert_eq!(surface.react_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn react_tool_missing_emoji_fails() {
        let surface = MockSurface::new();
        let tool = SlackReactTool::new(surface);
        let input = serde_json::json!({"channel": "C123", "timestamp": "100.001"});
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    // ── SlackReadTool tests ─────────────────────────────────────────

    #[test]
    fn read_tool_name_and_schema() {
        let surface = MockSurface::new();
        let tool = SlackReadTool::new(surface);
        assert_eq!(tool.name(), "slack.read");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["channel"]));
    }

    #[tokio::test]
    async fn read_tool_reads_messages() {
        let surface = MockSurface::new();
        let tool = SlackReadTool::new(Arc::clone(&surface) as Arc<dyn ChannelSurface>);
        let input = serde_json::json!({
            "channel": "C123"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["channel"], "C123");
        assert_eq!(result.output["count"], 1);
        assert_eq!(surface.read_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn read_tool_with_limit() {
        let surface = MockSurface::new();
        let tool = SlackReadTool::new(Arc::clone(&surface) as Arc<dyn ChannelSurface>);
        let input = serde_json::json!({
            "channel": "C123",
            "limit": 5
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn read_tool_caps_limit_at_100() {
        let surface = MockSurface::new();
        let tool = SlackReadTool::new(surface);
        let input = serde_json::json!({
            "channel": "C123",
            "limit": 999
        });
        // Should not error — limit is capped internally
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn read_tool_missing_channel_fails() {
        let surface = MockSurface::new();
        let tool = SlackReadTool::new(surface);
        let input = serde_json::json!({"limit": 10});
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_tool_without_range_skips_history_path() {
        let surface = MockSurface::new();
        let tool = SlackReadTool::new(Arc::clone(&surface) as Arc<dyn ChannelSurface>);
        let result = tool
            .execute(serde_json::json!({"channel": "C123"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.get("truncated").is_none());
        assert_eq!(surface.history_count.load(Ordering::SeqCst), 0);
        assert_eq!(surface.read_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn read_tool_with_oldest_reports_truncation_and_resume_cursor() {
        let surface = MockSurface::new();
        let tool = SlackReadTool::new(Arc::clone(&surface) as Arc<dyn ChannelSurface>);
        let result = tool
            .execute(serde_json::json!({"channel": "C123", "oldest": "50.000"}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"], 2);
        assert_eq!(result.output["truncated"], true);
        // Messages come newest-first; the resume cursor is the oldest returned ts.
        assert_eq!(result.output["next_latest"], "100.001");
        assert_eq!(surface.history_count.load(Ordering::SeqCst), 1);
        assert_eq!(surface.read_count.load(Ordering::SeqCst), 0);
    }

    use super::super::surface::{
        channel_summary_from_json, file_summary_from_json, message_match_from_json,
        message_reply_from_json, user_summary_from_json,
    };

    #[test]
    fn parser_channel_full_shape() {
        let json = serde_json::json!({
            "id": "C0123", "name": "support", "is_private": false, "is_archived": false,
            "is_member": true, "num_members": 42,
            "topic": {"value": "Customer support"}, "purpose": {"value": "Triage tickets"}
        });
        let c = channel_summary_from_json(&json);
        assert_eq!(c.id, "C0123");
        assert_eq!(c.name, "support");
        assert!(!c.is_private);
        assert!(c.is_member);
        assert_eq!(c.num_members, Some(42));
        assert_eq!(c.topic.as_deref(), Some("Customer support"));
        assert_eq!(c.purpose.as_deref(), Some("Triage tickets"));
    }

    #[test]
    fn parser_channel_im_falls_back_to_id_for_name_and_marks_private() {
        let json = serde_json::json!({"id": "D0001", "is_im": true});
        let c = channel_summary_from_json(&json);
        assert_eq!(c.name, "D0001");
        assert!(c.is_private, "IMs are private by Slack semantics");
        assert!(c.topic.is_none());
        assert!(c.purpose.is_none());
    }

    #[test]
    fn parser_channel_empty_topic_value_is_omitted() {
        let json = serde_json::json!({
            "id": "C0001", "name": "x", "topic": {"value": ""}, "purpose": {"value": ""}
        });
        let c = channel_summary_from_json(&json);
        assert!(c.topic.is_none());
        assert!(c.purpose.is_none());
    }

    #[test]
    fn parser_user_with_email() {
        let json = serde_json::json!({
            "id": "U001", "name": "kings", "real_name": "Kingsley", "deleted": false,
            "profile": {"display_name": "kings", "email": "k@x.com", "real_name": "Kingsley H"}
        });
        let u = user_summary_from_json(&json);
        assert_eq!(u.id, "U001");
        assert_eq!(u.email.as_deref(), Some("k@x.com"));
        assert_eq!(u.display_name.as_deref(), Some("kings"));
        assert_eq!(u.real_name.as_deref(), Some("Kingsley H"));
        assert!(!u.is_bot);
        assert!(!u.is_deleted);
    }

    #[test]
    fn parser_user_without_email_field_omits_email() {
        let json = serde_json::json!({
            "id": "U001", "name": "kings", "profile": {"display_name": "kings"}
        });
        let u = user_summary_from_json(&json);
        assert!(u.email.is_none());
    }

    #[test]
    fn parser_user_with_empty_email_omits_email() {
        let json = serde_json::json!({
            "id": "U001", "name": "kings", "profile": {"email": ""}
        });
        let u = user_summary_from_json(&json);
        assert!(u.email.is_none());
    }

    #[test]
    fn parser_user_bot_account() {
        let json = serde_json::json!({"id": "B001", "name": "botty", "is_bot": true});
        let u = user_summary_from_json(&json);
        assert!(u.is_bot);
        assert!(!u.is_deleted);
    }

    #[test]
    fn parser_user_deleted() {
        let json = serde_json::json!({"id": "U001", "name": "old", "deleted": true});
        let u = user_summary_from_json(&json);
        assert!(u.is_deleted);
    }

    #[test]
    fn parser_message_match() {
        let json = serde_json::json!({
            "channel": {"id": "C001", "name": "support"},
            "user": "U001", "username": "kings",
            "text": "support query", "ts": "1700000000.000100",
            "permalink": "https://x.slack.com/archives/C001/p123"
        });
        let m = message_match_from_json(&json);
        assert_eq!(m.channel_id, "C001");
        assert_eq!(m.channel_name.as_deref(), Some("support"));
        assert_eq!(m.text, "support query");
        assert_eq!(
            m.permalink.as_deref(),
            Some("https://x.slack.com/archives/C001/p123")
        );
    }

    #[test]
    fn parser_message_reply() {
        let json = serde_json::json!({"user": "U001", "text": "ack", "ts": "100.1"});
        let r = message_reply_from_json(&json);
        assert_eq!(r.user_id.as_deref(), Some("U001"));
        assert_eq!(r.text, "ack");
        assert_eq!(r.ts, "100.1");
    }

    #[test]
    fn parser_message_reply_composes_bot_attachment_content() {
        let json = serde_json::json!({
            "bot_id": "B_DD", "text": "", "ts": "100.2",
            "attachments": [{"title": "Alert recovered", "text": "all clear"}],
        });
        let r = message_reply_from_json(&json);
        assert_eq!(r.text, "Alert recovered\nall clear");
    }

    #[test]
    fn parser_message_match_composes_bot_attachment_content() {
        let json = serde_json::json!({
            "channel": {"id": "C001", "name": "monitoring"},
            "bot_id": "B_DD", "text": "", "ts": "100.3",
            "attachments": [{"fallback": "Disk 90% on host-1"}],
        });
        let m = message_match_from_json(&json);
        assert_eq!(m.text, "Disk 90% on host-1");
    }

    #[test]
    fn parser_file_summary() {
        let json = serde_json::json!({
            "id": "F001", "name": "doc.pdf", "title": "Doc",
            "mimetype": "application/pdf", "size": 1024,
            "user": "U001", "created": 1700000000,
            "permalink": "https://x.slack.com/files/F001"
        });
        let f = file_summary_from_json(&json);
        assert_eq!(f.id, "F001");
        assert_eq!(f.size, 1024);
        assert_eq!(f.user_id, "U001");
        assert_eq!(f.created, 1700000000);
    }

    fn ch(name: &str, topic: Option<&str>) -> SlackChannelSummary {
        SlackChannelSummary {
            id: "C0".into(),
            name: name.into(),
            is_private: false,
            is_archived: false,
            is_member: false,
            num_members: None,
            topic: topic.map(|s| s.to_string()),
            purpose: None,
        }
    }

    #[test]
    fn filter_channel_matches_name_substring_case_insensitive() {
        let c = ch("support-uk", None);
        assert!(match_channel(&c, Some("SUPPORT")));
        assert!(match_channel(&c, Some("uk")));
    }

    #[test]
    fn filter_channel_matches_topic() {
        let c = ch("random", Some("Triage incidents"));
        assert!(match_channel(&c, Some("triage")));
    }

    #[test]
    fn filter_channel_no_match_returns_false() {
        let c = ch("random", None);
        assert!(!match_channel(&c, Some("support")));
    }

    #[test]
    fn filter_channel_none_passes_everything() {
        let c = ch("anything", None);
        assert!(match_channel(&c, None));
    }

    fn usr(name: &str, email: Option<&str>) -> SlackUserSummary {
        SlackUserSummary {
            id: "U0".into(),
            name: name.into(),
            real_name: None,
            display_name: None,
            email: email.map(|s| s.to_string()),
            is_bot: false,
            is_deleted: false,
        }
    }

    #[test]
    fn filter_user_name_matches_real_or_display_or_email() {
        let u = SlackUserSummary {
            id: "U0".into(),
            name: "k".into(),
            real_name: Some("Kingsley".into()),
            display_name: Some("kings".into()),
            email: Some("k@x.com".into()),
            is_bot: false,
            is_deleted: false,
        };
        assert!(user_haystack_contains(&u, "kingsley"));
        assert!(user_haystack_contains(&u, "kings"));
        assert!(user_haystack_contains(&u, "k@"));
        assert!(!user_haystack_contains(&u, "absent"));
    }

    #[test]
    fn email_filter_only_matches_email_field() {
        let u = usr("kingsley", Some("k@x.com"));
        assert!(email_contains(&u, "k@"));
        assert!(!email_contains(&u, "kingsley"));
    }

    use super::super::surface::{clamp_limit, parse_next_cursor, scope_hint_for};

    #[test]
    fn clamp_limit_bounds() {
        assert_eq!(clamp_limit(0), 1);
        assert_eq!(clamp_limit(5000), 1000);
        assert_eq!(clamp_limit(500), 500);
    }

    #[test]
    fn parse_next_cursor_empty_string_is_none() {
        let v = serde_json::json!({"response_metadata": {"next_cursor": ""}});
        assert!(parse_next_cursor(&v).is_none());
    }

    #[test]
    fn parse_next_cursor_present_returns_some() {
        let v = serde_json::json!({"response_metadata": {"next_cursor": "abc"}});
        assert_eq!(parse_next_cursor(&v).as_deref(), Some("abc"));
    }

    #[test]
    fn parse_next_cursor_absent_is_none() {
        let v = serde_json::json!({});
        assert!(parse_next_cursor(&v).is_none());
    }

    #[test]
    fn scope_hint_for_known_methods() {
        assert!(scope_hint_for("users.list", None).contains("users:read"));
        assert!(scope_hint_for("users.lookupByEmail", None).contains("users:read.email"));
        assert!(scope_hint_for("conversations.create", None).contains("channels:manage"));
        assert!(scope_hint_for("search.messages", None).contains("search:read"));
        assert!(scope_hint_for("files.delete", None).contains("files:write"));
        assert!(scope_hint_for("pins.add", None).contains("pins:write"));
    }

    #[test]
    fn scope_hint_for_conversations_list_multi_type() {
        let hint = scope_hint_for("conversations.list", Some("public_channel,private_channel"));
        assert!(hint.contains("channels:read"));
        assert!(hint.contains("groups:read"));
    }

    #[test]
    fn scope_hint_for_conversations_list_public_only() {
        assert_eq!(
            scope_hint_for("conversations.list", Some("public_channel")),
            "channels:read"
        );
    }

    use wiremock::matchers::{
        body_partial_json, method, path, query_param, query_param_is_missing,
    };
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn fixture() -> (MockServer, Arc<SlackSurface>) {
        let server = MockServer::start().await;
        let surface = Arc::new(SlackSurface::with_base_url("xoxb-test", server.uri()));
        (server, surface)
    }

    async fn fixture_with_user_token() -> (MockServer, Arc<SlackSurface>) {
        let server = MockServer::start().await;
        let store: Arc<dyn workforce_common::settings::SecretStore> =
            crate::adapters::slack::surface::tests::LayeredFake::new(
                &[],
                &[("users/kingsley/skills/slack/test/user_token", "xoxp-test")],
            );
        let surface = Arc::new(SlackSurface::with_store(
            "xoxb-test",
            store,
            "test",
            server.uri(),
        ));
        (server, surface)
    }

    async fn run_as_user<F: std::future::Future>(uid: &str, fut: F) -> F::Output {
        workforce_agent::runner::CURRENT_USER_ID
            .scope(uid.to_string(), fut)
            .await
    }

    fn ok(body: serde_json::Value) -> ResponseTemplate {
        let mut payload = body;
        payload["ok"] = serde_json::json!(true);
        ResponseTemplate::new(200).set_body_json(payload)
    }

    fn slack_err(code: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": false,
            "error": code,
        }))
    }

    #[tokio::test]
    async fn list_channels_schema() {
        let tool = SlackListChannelsTool::new(Arc::new(SlackSurface::new("x")));
        assert_eq!(tool.name(), "slack.list_channels");
        let schema = tool.input_schema();
        assert!(schema["properties"]["filter"].is_object());
        assert!(schema["properties"]["limit"].is_object());
    }

    #[tokio::test]
    async fn list_channels_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/conversations.list"))
            .respond_with(ok(serde_json::json!({
                "channels": [
                    {"id": "C0", "name": "support", "is_private": false,
                     "is_archived": false, "is_member": true, "num_members": 5,
                     "topic": {"value": "Support"}, "purpose": {"value": ""}}
                ],
                "response_metadata": {"next_cursor": ""}
            })))
            .mount(&server)
            .await;
        let tool = SlackListChannelsTool::new(surface);
        let res = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(res.success, "{res:?}");
        assert_eq!(res.output["count"], 1);
        assert_eq!(res.output["channels"][0]["name"], "support");
        assert!(res.output["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn list_channels_filter_narrows() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/conversations.list"))
            .respond_with(ok(serde_json::json!({
                "channels": [
                    {"id":"C1","name":"support-uk","is_private":false,"is_archived":false,"is_member":true},
                    {"id":"C2","name":"random","is_private":false,"is_archived":false,"is_member":false},
                    {"id":"C3","name":"Support-US","is_private":false,"is_archived":false,"is_member":true}
                ],
                "response_metadata": {"next_cursor": ""}
            })))
            .mount(&server)
            .await;
        let tool = SlackListChannelsTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"filter": "support"}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["count"], 2);
    }

    #[tokio::test]
    async fn list_channels_missing_scope_names_scope() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/conversations.list"))
            .respond_with(slack_err("missing_scope"))
            .mount(&server)
            .await;
        let tool = SlackListChannelsTool::new(surface);
        let res = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!res.success);
        let err = res.error.unwrap_or_default();
        assert!(err.contains("channels:read"), "err: {err}");
    }

    #[tokio::test]
    async fn list_channels_cursor_passes_through() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/conversations.list"))
            .and(query_param("cursor", "abc"))
            .respond_with(ok(serde_json::json!({
                "channels": [], "response_metadata": {"next_cursor": "def"}
            })))
            .mount(&server)
            .await;
        let tool = SlackListChannelsTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"cursor": "abc"}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["next_cursor"], "def");
    }

    #[tokio::test]
    async fn list_users_excludes_bots_and_deleted_by_default() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/users.list"))
            .respond_with(ok(serde_json::json!({
                "members": [
                    {"id":"U1","name":"a","deleted":false,"is_bot":false},
                    {"id":"U2","name":"b","deleted":true,"is_bot":false},
                    {"id":"U3","name":"c","deleted":false,"is_bot":true}
                ],
                "response_metadata": {"next_cursor": ""}
            })))
            .mount(&server)
            .await;
        let tool = SlackListUsersTool::new(surface);
        let res = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(res.success);
        assert_eq!(res.output["count"], 1);
        assert_eq!(res.output["users"][0]["id"], "U1");
    }

    #[tokio::test]
    async fn list_users_email_filter_matches_email_only() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/users.list"))
            .respond_with(ok(serde_json::json!({
                "members": [
                    {"id":"U1","name":"kingsley","profile":{"email":"a@b.com"}},
                    {"id":"U2","name":"noemail","profile":{"email":"kingsley@x.com"}}
                ],
                "response_metadata": {"next_cursor": ""}
            })))
            .mount(&server)
            .await;
        let tool = SlackListUsersTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"email": "kingsley"}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["count"], 1);
        assert_eq!(res.output["users"][0]["id"], "U2");
    }

    #[tokio::test]
    async fn list_users_walks_until_filter_matches() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/users.list"))
            .and(query_param_is_missing("cursor"))
            .respond_with(ok(serde_json::json!({
                "members": [{"id":"U1","name":"alice"}],
                "response_metadata": {"next_cursor": "p2"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/users.list"))
            .and(query_param("cursor", "p2"))
            .respond_with(ok(serde_json::json!({
                "members": [{"id":"U2","name":"bob"}],
                "response_metadata": {"next_cursor": "p3"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/users.list"))
            .and(query_param("cursor", "p3"))
            .respond_with(ok(serde_json::json!({
                "members": [{"id":"U3","name":"kingsley"}],
                "response_metadata": {"next_cursor": ""}
            })))
            .mount(&server)
            .await;
        let tool = SlackListUsersTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"filter": "kingsley"}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["count"], 1);
        assert_eq!(res.output["users"][0]["id"], "U3");
        assert_eq!(res.output["truncated"], false);
        assert!(res.output["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn list_users_stops_at_match_cap() {
        let (server, surface) = fixture().await;
        let members: Vec<serde_json::Value> = (0..MAX_FIND_MATCHES + 5)
            .map(|i| serde_json::json!({"id": format!("U{i}"), "name": format!("kingsley-{i}")}))
            .collect();
        Mock::given(method("GET"))
            .and(path("/users.list"))
            .respond_with(ok(serde_json::json!({
                "members": members,
                "response_metadata": {"next_cursor": "more"}
            })))
            .mount(&server)
            .await;
        let tool = SlackListUsersTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"filter": "kingsley"}))
            .await
            .unwrap();
        assert!(res.success);
        assert!(res.output["count"].as_u64().unwrap() >= MAX_FIND_MATCHES as u64);
        assert_eq!(res.output["truncated"], true);
        assert_eq!(res.output["next_cursor"], "more");
    }

    #[tokio::test]
    async fn list_users_stops_at_page_budget() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/users.list"))
            .respond_with(ok(serde_json::json!({
                "members": [{"id":"U1","name":"alice"}],
                "response_metadata": {"next_cursor": "next"}
            })))
            .expect(MAX_FIND_PAGES as u64)
            .mount(&server)
            .await;
        let tool = SlackListUsersTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"filter": "zelda"}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["count"], 0);
        assert_eq!(res.output["truncated"], true);
        assert_eq!(res.output["next_cursor"], "next");
    }

    #[tokio::test]
    async fn list_users_no_filter_does_single_page() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/users.list"))
            .respond_with(ok(serde_json::json!({
                "members": [{"id":"U1","name":"alice"}, {"id":"U2","name":"bob"}],
                "response_metadata": {"next_cursor": "more"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let tool = SlackListUsersTool::new(surface);
        let res = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(res.success);
        assert_eq!(res.output["count"], 2);
        assert_eq!(res.output["next_cursor"], "more");
        assert_eq!(res.output["truncated"], true);
    }

    #[tokio::test]
    async fn list_users_filter_resumes_from_cursor() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/users.list"))
            .and(query_param("cursor", "resume-from"))
            .respond_with(ok(serde_json::json!({
                "members": [{"id":"U9","name":"kings"}],
                "response_metadata": {"next_cursor": ""}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let tool = SlackListUsersTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"filter": "kings", "cursor": "resume-from"}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["count"], 1);
        assert_eq!(res.output["users"][0]["id"], "U9");
    }

    #[tokio::test]
    async fn list_users_empty_string_filter_skips_walk() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/users.list"))
            .respond_with(ok(serde_json::json!({
                "members": [{"id":"U1","name":"alice"}],
                "response_metadata": {"next_cursor": "more"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let tool = SlackListUsersTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"filter": "", "email": ""}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["next_cursor"], "more");
    }

    #[tokio::test]
    async fn lookup_user_by_email_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/users.lookupByEmail"))
            .and(query_param("email", "k@x.com"))
            .respond_with(ok(serde_json::json!({
                "user": {"id": "U1", "name": "kings", "profile": {"email": "k@x.com"}}
            })))
            .mount(&server)
            .await;
        let tool = SlackLookupUserByEmailTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"email": "k@x.com"}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["user"]["id"], "U1");
    }

    #[tokio::test]
    async fn lookup_user_by_email_not_found() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/users.lookupByEmail"))
            .respond_with(slack_err("users_not_found"))
            .mount(&server)
            .await;
        let tool = SlackLookupUserByEmailTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"email": "x@x.com"}))
            .await
            .unwrap();
        assert!(!res.success);
        let err = res.error.unwrap_or_default();
        assert!(err.contains("x@x.com"), "err: {err}");
    }

    #[tokio::test]
    async fn list_channel_members_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/conversations.members"))
            .and(query_param("channel", "C0"))
            .respond_with(ok(serde_json::json!({
                "members": ["U1", "U2", "U3"], "response_metadata": {"next_cursor": ""}
            })))
            .mount(&server)
            .await;
        let tool = SlackListChannelMembersTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0"}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["count"], 3);
        assert_eq!(res.output["members"][0], "U1");
    }

    #[tokio::test]
    async fn search_messages_happy_path_uses_per_user_token() {
        let (server, surface) = fixture_with_user_token().await;
        Mock::given(method("GET"))
            .and(path("/search.messages"))
            .and(query_param("query", "incident"))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer xoxp-test",
            ))
            .respond_with(ok(serde_json::json!({
                "messages": {
                    "matches": [
                        {"channel": {"id":"C0", "name":"ops"}, "user":"U1", "text":"incident now",
                         "ts":"100.1", "permalink": "https://x"}
                    ],
                    "total": 5
                },
                "response_metadata": {"next_cursor": ""}
            })))
            .mount(&server)
            .await;
        let tool = SlackSearchMessagesTool::new(surface);
        let res = run_as_user(
            "kingsley",
            tool.execute(serde_json::json!({"query": "incident"})),
        )
        .await
        .unwrap();
        assert!(res.success, "err: {:?}", res.error);
        assert_eq!(res.output["count"], 1);
        assert_eq!(res.output["total"], 5);
        assert_eq!(res.output["matches"][0]["channel_id"], "C0");
    }

    #[tokio::test]
    async fn search_messages_paid_only_surfaces_through_user_token_path() {
        let (server, surface) = fixture_with_user_token().await;
        Mock::given(method("GET"))
            .and(path("/search.messages"))
            .respond_with(slack_err("paid_only"))
            .mount(&server)
            .await;
        let tool = SlackSearchMessagesTool::new(surface);
        let res = run_as_user(
            "kingsley",
            tool.execute(serde_json::json!({"query": "incident"})),
        )
        .await
        .unwrap();
        assert!(!res.success);
        let err = res.error.unwrap_or_default();
        assert!(err.contains("paid"), "err: {err}");
    }

    #[tokio::test]
    async fn search_messages_pre_flight_error_when_no_user_identity() {
        let (_server, surface) = fixture_with_user_token().await;
        let tool = SlackSearchMessagesTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"query": "incident"}))
            .await
            .unwrap();
        assert!(!res.success);
        let err = res.error.unwrap_or_default();
        assert!(err.contains("no user identity"), "err: {err}");
    }

    #[tokio::test]
    async fn search_messages_pre_flight_error_when_no_token_registered() {
        let (_server, surface) = fixture_with_user_token().await;
        let tool = SlackSearchMessagesTool::new(surface);
        let res = run_as_user(
            "brian",
            tool.execute(serde_json::json!({"query": "incident"})),
        )
        .await
        .unwrap();
        assert!(!res.success);
        let err = res.error.unwrap_or_default();
        assert!(err.contains("slack.user_token_set"), "err: {err}");
        assert!(err.contains("brian"), "err: {err}");
    }

    #[tokio::test]
    async fn get_message_replies_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/conversations.replies"))
            .respond_with(ok(serde_json::json!({
                "messages": [
                    {"user":"U1", "text":"parent", "ts":"100.0"},
                    {"user":"U2", "text":"reply", "ts":"101.0"}
                ],
                "has_more": false,
                "response_metadata": {"next_cursor": ""}
            })))
            .mount(&server)
            .await;
        let tool = SlackGetMessageRepliesTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0", "ts": "100.0"}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["count"], 2);
        assert_eq!(res.output["has_more"], false);
    }

    #[tokio::test]
    async fn user_info_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/users.info"))
            .and(query_param("user", "U0"))
            .respond_with(ok(serde_json::json!({
                "user": {"id":"U0","name":"k","profile":{"display_name":"kings"}}
            })))
            .mount(&server)
            .await;
        let tool = SlackUserInfoTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"user": "U0"}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["user"]["id"], "U0");
    }

    #[tokio::test]
    async fn list_user_conversations_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/users.conversations"))
            .respond_with(ok(serde_json::json!({
                "channels": [
                    {"id":"C0","name":"ops","is_private":false,"is_archived":false,"is_member":true}
                ],
                "response_metadata": {"next_cursor": ""}
            })))
            .mount(&server)
            .await;
        let tool = SlackListUserConversationsTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"user": "U0"}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["count"], 1);
    }

    #[tokio::test]
    async fn invite_to_channel_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/conversations.invite"))
            .and(body_partial_json(
                serde_json::json!({"channel": "C0", "users": "U1,U2"}),
            ))
            .respond_with(ok(serde_json::json!({
                "channel": {"id":"C0","name":"ops"}
            })))
            .mount(&server)
            .await;
        let tool = SlackInviteToChannelTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0", "users": ["U1", "U2"]}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["invited_count"], 2);
    }

    #[tokio::test]
    async fn invite_to_channel_rejects_empty_users_list() {
        let surface = Arc::new(SlackSurface::new("x"));
        let tool = SlackInviteToChannelTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0", "users": []}))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap_or_default().contains("non-empty"));
    }

    #[tokio::test]
    async fn update_message_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/chat.update"))
            .and(body_partial_json(serde_json::json!({"text": "edited"})))
            .respond_with(ok(serde_json::json!({})))
            .mount(&server)
            .await;
        let tool = SlackUpdateMessageTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0", "ts": "100.0", "text": "edited"}))
            .await
            .unwrap();
        assert!(res.success);
    }

    #[tokio::test]
    async fn update_message_cant_update() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/chat.update"))
            .respond_with(slack_err("cant_update_message"))
            .mount(&server)
            .await;
        let tool = SlackUpdateMessageTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel":"C0","ts":"100.0","text":"x"}))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap_or_default().contains("only modify"));
    }

    #[tokio::test]
    async fn delete_message_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/chat.delete"))
            .respond_with(ok(serde_json::json!({})))
            .mount(&server)
            .await;
        let tool = SlackDeleteMessageTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0", "ts": "100.0"}))
            .await
            .unwrap();
        assert!(res.success);
    }

    #[tokio::test]
    async fn pin_message_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/pins.add"))
            .respond_with(ok(serde_json::json!({})))
            .mount(&server)
            .await;
        let tool = SlackPinMessageTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0", "ts": "100.0"}))
            .await
            .unwrap();
        assert!(res.success);
    }

    #[tokio::test]
    async fn unpin_message_already_unpinned() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/pins.remove"))
            .respond_with(slack_err("no_pin"))
            .mount(&server)
            .await;
        let tool = SlackUnpinMessageTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0", "ts": "100.0"}))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap_or_default().contains("not pinned"));
    }

    #[tokio::test]
    async fn create_channel_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/conversations.create"))
            .respond_with(ok(serde_json::json!({
                "channel": {"id": "C0NEW", "name": "agent-test", "is_private": false}
            })))
            .mount(&server)
            .await;
        let tool = SlackCreateChannelTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"name": "agent-test"}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["channel"]["id"], "C0NEW");
    }

    #[tokio::test]
    async fn create_channel_name_taken() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/conversations.create"))
            .respond_with(slack_err("name_taken"))
            .mount(&server)
            .await;
        let tool = SlackCreateChannelTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"name": "agent-test"}))
            .await
            .unwrap();
        assert!(!res.success);
        let err = res.error.unwrap_or_default();
        assert!(err.contains("agent-test"), "err: {err}");
        assert!(err.contains("already exists"));
    }

    #[tokio::test]
    async fn archive_channel_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/conversations.archive"))
            .respond_with(ok(serde_json::json!({})))
            .mount(&server)
            .await;
        let tool = SlackArchiveChannelTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0"}))
            .await
            .unwrap();
        assert!(res.success);
    }

    #[tokio::test]
    async fn archive_channel_cant_archive_general() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/conversations.archive"))
            .respond_with(slack_err("cant_archive_general"))
            .mount(&server)
            .await;
        let tool = SlackArchiveChannelTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0"}))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap_or_default().contains("#general"));
    }

    #[tokio::test]
    async fn unarchive_channel_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/conversations.unarchive"))
            .respond_with(ok(serde_json::json!({})))
            .mount(&server)
            .await;
        let tool = SlackUnarchiveChannelTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0"}))
            .await
            .unwrap();
        assert!(res.success);
    }

    #[tokio::test]
    async fn set_channel_metadata_rejects_no_fields() {
        let surface = Arc::new(SlackSurface::new("x"));
        let tool = SlackSetChannelMetadataTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0"}))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap_or_default().contains("at least one"));
    }

    #[tokio::test]
    async fn set_channel_metadata_serial_topic_then_purpose() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/conversations.setTopic"))
            .respond_with(ok(serde_json::json!({})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/conversations.setPurpose"))
            .respond_with(ok(serde_json::json!({})))
            .mount(&server)
            .await;
        let tool = SlackSetChannelMetadataTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel":"C0","topic":"t","purpose":"p"}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["topic_set"], true);
        assert_eq!(res.output["purpose_set"], true);
    }

    #[tokio::test]
    async fn set_channel_metadata_topic_fail_aborts_purpose() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/conversations.setTopic"))
            .respond_with(slack_err("is_archived"))
            .mount(&server)
            .await;
        let tool = SlackSetChannelMetadataTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel":"C0","topic":"t","purpose":"p"}))
            .await
            .unwrap();
        assert!(!res.success);
    }

    #[tokio::test]
    async fn rename_channel_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/conversations.rename"))
            .respond_with(ok(serde_json::json!({
                "channel": {"id": "C0", "name": "new-name", "is_private": false}
            })))
            .mount(&server)
            .await;
        let tool = SlackRenameChannelTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0", "name": "new-name"}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["channel"]["name"], "new-name");
    }

    #[tokio::test]
    async fn kick_from_channel_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/conversations.kick"))
            .respond_with(ok(serde_json::json!({})))
            .mount(&server)
            .await;
        let tool = SlackKickFromChannelTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0", "user": "U1"}))
            .await
            .unwrap();
        assert!(res.success);
    }

    #[tokio::test]
    async fn kick_from_channel_cant_kick_self() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/conversations.kick"))
            .respond_with(slack_err("cant_kick_self"))
            .mount(&server)
            .await;
        let tool = SlackKickFromChannelTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"channel": "C0", "user": "U_BOT"}))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap_or_default().contains("itself"));
    }

    #[tokio::test]
    async fn upload_file_rejects_both_text_and_base64() {
        let surface = Arc::new(SlackSurface::new("x"));
        let tool = SlackUploadFileTool::new(surface);
        let res = tool
            .execute(serde_json::json!({
                "filename":"a.txt","content_text":"hi","content_base64":"aGk="
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap_or_default().contains("exactly one"));
    }

    #[tokio::test]
    async fn upload_file_rejects_neither_content() {
        let surface = Arc::new(SlackSurface::new("x"));
        let tool = SlackUploadFileTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"filename": "a.txt"}))
            .await
            .unwrap();
        assert!(!res.success);
    }

    #[tokio::test]
    async fn upload_file_rejects_empty_content() {
        let surface = Arc::new(SlackSurface::new("x"));
        let tool = SlackUploadFileTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"filename":"a.txt","content_text":""}))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap_or_default().contains("empty"));
    }

    #[tokio::test]
    async fn upload_file_rejects_invalid_base64() {
        let surface = Arc::new(SlackSurface::new("x"));
        let tool = SlackUploadFileTool::new(surface);
        let res = tool
            .execute(serde_json::json!({
                "filename":"a.txt","content_base64":"!!!not-base64!!!"
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap_or_default().contains("base64"));
    }

    #[tokio::test]
    async fn upload_file_two_step_happy_path() {
        let (server, surface) = fixture().await;
        let upload_url = format!("{}/_upload_target", server.uri());
        Mock::given(method("POST"))
            .and(path("/files.getUploadURLExternal"))
            .respond_with(ok(serde_json::json!({
                "upload_url": upload_url, "file_id": "F0"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/_upload_target"))
            .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/files.completeUploadExternal"))
            .respond_with(ok(serde_json::json!({
                "files": [{"id":"F0", "permalink":"https://x", "title":"a.txt"}]
            })))
            .mount(&server)
            .await;
        let tool = SlackUploadFileTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"filename":"a.txt","content_text":"hi"}))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert_eq!(res.output["file"]["file_id"], "F0");
        assert_eq!(res.output["file"]["permalink"], "https://x");
    }

    #[tokio::test]
    async fn list_files_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/files.list"))
            .respond_with(ok(serde_json::json!({
                "files": [
                    {"id":"F1","name":"x.pdf","title":"X","mimetype":"application/pdf",
                     "size":100,"user":"U1","created":1700000000,"permalink":"https://y"}
                ],
                "response_metadata": {"next_cursor": ""}
            })))
            .mount(&server)
            .await;
        let tool = SlackListFilesTool::new(surface);
        let res = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(res.success);
        assert_eq!(res.output["count"], 1);
    }

    #[tokio::test]
    async fn delete_file_happy_path() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/files.delete"))
            .respond_with(ok(serde_json::json!({})))
            .mount(&server)
            .await;
        let tool = SlackDeleteFileTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"file": "F0"}))
            .await
            .unwrap();
        assert!(res.success);
    }

    #[tokio::test]
    async fn delete_file_not_found() {
        let (server, surface) = fixture().await;
        Mock::given(method("POST"))
            .and(path("/files.delete"))
            .respond_with(slack_err("file_not_found"))
            .mount(&server)
            .await;
        let tool = SlackDeleteFileTool::new(surface);
        let res = tool
            .execute(serde_json::json!({"file": "F0"}))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap_or_default().contains("F0"));
    }

    #[test]
    fn new_slack_tool_names_match_registration_count() {
        assert_eq!(super::super::NEW_SLACK_TOOL_NAMES.len(), 22);
    }
}

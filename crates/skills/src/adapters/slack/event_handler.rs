//! Slack event handler — parses events and produces [`SlackAction`] values.
//!
//! Decoupled from the agent service: the handler only decides *what* should
//! happen, not *how* to execute it. The hub's orchestration layer consumes
//! actions and dispatches to the appropriate agent.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use lru::LruCache;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, warn};

use workforce_common::channels::{ConversationVisibility, InboundEvent};
use workforce_common::engagement::EngagementOverrides;
use workforce_common::persona::PersonaOverrides;
use workforce_common::settings::{ResponseMode, SlackChannelOverride};

use super::inbound::InboundContent;

/// Bounded set of recently-processed message identities (`channel:ts`), used to
/// drop the duplicate agent run Slack triggers in channels and multi-person DMs:
/// an @mention there delivers *both* a `message` and an `app_mention` event for
/// the same message — same `ts`, different `envelope_id` — so the transport's
/// envelope-id dedup cannot catch the pair. The LRU bounds memory.
const MESSAGE_DEDUP_CAPACITY: usize = 4096;

/// Slack event payload (simplified).
#[derive(Debug, Clone, Deserialize)]
pub struct SlackEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub subtype: Option<String>,
    pub user: Option<String>,
    pub text: Option<String>,
    pub channel: Option<String>,
    pub channel_type: Option<String>,
    pub ts: Option<String>,
    pub thread_ts: Option<String>,
    pub bot_id: Option<String>,
    pub reaction: Option<String>,
    pub item: Option<SlackReactionItem>,
    #[serde(default, deserialize_with = "lenient_files")]
    pub files: Option<Vec<SlackFile>>,
    /// Raw Block Kit blocks. Kept as a raw `Value` so an unexpected shape
    /// can never fail the whole event parse.
    #[serde(default)]
    pub blocks: Value,
    /// Attachments: integration alert bodies, forwards, pasted tables.
    #[serde(default)]
    pub attachments: Value,
    /// Nested message for `message_changed` subtype.
    pub message: Option<Box<SlackEvent>>,
    /// Previous message for `message_changed` subtype.
    pub previous_message: Option<Box<SlackEvent>>,
    /// Deleted timestamp for `message_deleted` subtype.
    pub deleted_ts: Option<String>,
}

/// A file attached to a Slack message.
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "Value")]
pub struct SlackFile {
    pub id: String,
    pub name: String,
    pub mimetype: String,
    pub size: u64,
    pub url_private_download: Option<String>,
}

/// Slack sends file stubs — hidden by a plan limit, deleted, shared across
/// workspaces — that carry little more than an `id`, so every field is read
/// leniently: a stub must still surface as a file the agent is told about,
/// not fail the event and take the message's text down with it.
impl From<Value> for SlackFile {
    fn from(file: Value) -> Self {
        let text = |key: &str| file[key].as_str().map(str::trim).filter(|s| !s.is_empty());
        Self {
            id: text("id").unwrap_or_default().to_string(),
            name: text("name")
                .or_else(|| text("title"))
                .unwrap_or("file")
                .to_string(),
            mimetype: text("mimetype").unwrap_or_default().to_string(),
            size: file["size"].as_u64().unwrap_or_default(),
            url_private_download: text("url_private_download").map(str::to_string),
        }
    }
}

/// `files` in any shape other than a list reads as absent rather than
/// failing the event.
fn lenient_files<'de, D>(deserializer: D) -> Result<Option<Vec<SlackFile>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Value::deserialize(deserializer)? {
        Value::Array(entries) => Some(entries.into_iter().map(SlackFile::from).collect()),
        _ => None,
    })
}

/// The item a reaction was added to.
#[derive(Debug, Clone, Deserialize)]
pub struct SlackReactionItem {
    #[serde(rename = "type")]
    pub item_type: String,
    pub channel: Option<String>,
    pub ts: Option<String>,
}

/// Actions produced by the event handler for the orchestration layer.
#[derive(Debug)]
pub enum SlackAction {
    /// A message that should be processed by an agent.
    ProcessMessage {
        workspace_id: String,
        session_key: String,
        user: String,
        /// What the user typed, in Slack wire form with the bot self-mention
        /// stripped — the input to command, credential and intent parsing.
        text: String,
        /// Original text before bot self-mention was stripped.
        /// Used by the channel coordinator to detect which agent was @mentioned.
        raw_text: String,
        /// What the agent reads: the typed text plus everything else the
        /// message carries (tables, non-text blocks, attachments).
        body: String,
        /// Messages this one forwards or quotes, each a framed quote. Written
        /// by someone other than the sender, so kept apart from `body`.
        shared: Vec<String>,
        channel: String,
        thread_ts: Option<String>,
        message_ts: Option<String>,
        is_dm: bool,
        is_mentioned: bool,
        /// Per-channel system prompt (from channel_overrides config).
        system_prompt: Option<String>,
        /// Friendly channel display name (from channel_overrides config).
        channel_display_name: Option<String>,
        /// Files attached to the message.
        files: Vec<SlackFile>,
    },
    /// A reaction was added to a message.
    ReactionAdded {
        workspace_id: String,
        user: String,
        channel: String,
        message_ts: String,
        reaction: String,
    },
    /// A reaction was removed from a message.
    ReactionRemoved {
        workspace_id: String,
        user: String,
        channel: String,
        message_ts: String,
        reaction: String,
    },
    /// A message was edited.
    MessageChanged {
        workspace_id: String,
        channel: String,
        user: String,
        old_text: String,
        new_text: String,
        message_ts: String,
    },
    /// A message was deleted.
    MessageDeleted {
        workspace_id: String,
        channel: String,
        deleted_ts: String,
    },
    /// A user joined a channel.
    MemberJoinedChannel {
        workspace_id: String,
        user: String,
        channel: String,
    },
    /// A user left a channel.
    MemberLeftChannel {
        workspace_id: String,
        user: String,
        channel: String,
    },
    /// A slash command was invoked.
    SlashCommand {
        workspace_id: String,
        session_key: String,
        user: String,
        command: String,
        text: String,
        channel: String,
        response_url: String,
        /// True when invoked from a direct message. Slash-command payloads
        /// lack `channel_type`, so we infer from Slack's channel-id prefix
        /// (`D` = DM, `C`/`G` = public/private channel).
        is_dm: bool,
    },
    /// A Block Kit interactive element (button, static_select, etc.) was triggered.
    InteractiveAction {
        workspace_id: String,
        user: String,
        /// The Slack team of the person who clicked, when Slack says: a
        /// click from another workspace (Slack Connect) carries its own.
        user_team: Option<String>,
        channel: String,
        /// `thread_ts` of the message the interactive element belonged to.
        thread_ts: Option<String>,
        /// `ts` of the message itself.
        message_ts: Option<String>,
        /// `action_id` assigned to the element (e.g. `"cc_stop"`).
        action_id: String,
        /// The element's `value` (free-form, used to carry the session key).
        value: Option<String>,
        /// Block Kit `blocks` of the message the element belonged to, echoed
        /// back by Slack. Lets a handler rebuild the message in place (e.g.
        /// strip a resolved approval's buttons) without re-deriving the prompt.
        message_blocks: Option<Value>,
        /// Slack-provided URL for async responses (good for 30 minutes).
        response_url: String,
    },
    /// Event was ignored (filtered out).
    Ignore,
}

/// Handles incoming Slack events and produces actions.
pub struct SlackEventHandler {
    workspace_id: String,
    bot_user_id: String,
    /// Matches the bot's own mention in either wire form (`<@UBOT>` or
    /// `<@UBOT|name>`); `None` until the bot's user id is known.
    self_mention: Option<regex::Regex>,
    /// Workspace/channel → agent bindings and the registry default, used to
    /// scope a runtime engagement override to the agent that actually serves
    /// the message (mirrors the dispatcher's channel→workspace→default routing).
    agent_bindings: HashMap<String, String>,
    default_agent_id: Option<String>,
    response_mode: ResponseMode,
    activation_keywords: Vec<String>,
    thread_isolation: bool,
    channel_allowlist: Vec<String>,
    channel_overrides: HashMap<String, SlackChannelOverride>,
    engagement_overrides: Option<Arc<EngagementOverrides>>,
    /// Independent tap feeding inbound messages to the channel-monitor engine —
    /// fired for every message (including other bots') before the chat-loop
    /// self/bot/should-respond gates; the engine's own loop guard excludes our
    /// output.
    monitor_tx: Option<mpsc::Sender<InboundEvent>>,
    /// Channel persona overrides (a derived, hot layer from `persona` monitors):
    /// a channel-scoped system prompt + response mode applied at message time.
    persona_overrides: Option<Arc<PersonaOverrides>>,
    seen_messages: Arc<Mutex<LruCache<String, ()>>>,
}

/// The readable content of a message event, in Slack wire form.
fn event_content(event: &SlackEvent, is_bot: bool) -> InboundContent {
    super::inbound::compose(
        event.text.as_deref().unwrap_or(""),
        is_bot,
        &event.blocks,
        &event.attachments,
    )
}

/// The agent that serves a channel message — its channel binding, else the
/// workspace binding, else the registry default. Mirrors the dispatcher's
/// non-DM routing (`resolve_agent_for_slack` → `AgentRegistry::resolve`) so a
/// runtime engagement override is read against the same agent whose `set_mode`
/// wrote it. DMs always respond regardless of mode, so they need no scope here.
fn serving_agent(
    bindings: &HashMap<String, String>,
    channel: &str,
    workspace_id: &str,
    default_agent_id: Option<&str>,
) -> Option<String> {
    bindings
        .get(channel)
        .or_else(|| bindings.get(workspace_id))
        .cloned()
        .or_else(|| default_agent_id.map(str::to_string))
}

/// Record `key` in the seen-set, returning whether it was already present. A
/// first sight records it and returns `false`; a repeat returns `true` so the
/// caller can drop the duplicate.
fn record_seen(seen: &mut LruCache<String, ()>, key: String) -> bool {
    if seen.contains(&key) {
        true
    } else {
        seen.put(key, ());
        false
    }
}

impl SlackEventHandler {
    pub fn new(
        workspace_id: String,
        bot_user_id: String,
        response_mode: ResponseMode,
        activation_keywords: Vec<String>,
        thread_isolation: bool,
        channel_allowlist: Vec<String>,
    ) -> Self {
        let self_mention = (!bot_user_id.is_empty())
            .then(|| {
                regex::Regex::new(&format!(r"<@{}(?:\|[^<>]*)?>", regex::escape(&bot_user_id)))
            })
            .and_then(Result::ok);
        Self {
            workspace_id,
            bot_user_id,
            self_mention,
            agent_bindings: HashMap::new(),
            default_agent_id: None,
            response_mode,
            activation_keywords,
            thread_isolation,
            channel_allowlist,
            channel_overrides: HashMap::new(),
            engagement_overrides: None,
            monitor_tx: None,
            persona_overrides: None,
            seen_messages: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(MESSAGE_DEDUP_CAPACITY).expect("capacity ≥ 1"),
            ))),
        }
    }

    /// True when a respondable message with this `channel:ts` identity has
    /// already been seen — the second half of Slack's `message`/`app_mention`
    /// pair (or a redelivery surviving a transport reconnect).
    async fn is_duplicate(&self, channel: &str, ts: &str) -> bool {
        let mut seen = self.seen_messages.lock().await;
        record_seen(&mut seen, format!("{channel}:{ts}"))
    }

    /// Create with per-channel configuration overrides.
    pub fn with_channel_overrides(
        mut self,
        overrides: HashMap<String, SlackChannelOverride>,
    ) -> Self {
        self.channel_overrides = overrides;
        self
    }

    /// Attach runtime engagement overrides for dynamic mode changes.
    pub fn with_engagement_overrides(mut self, overrides: Arc<EngagementOverrides>) -> Self {
        self.engagement_overrides = Some(overrides);
        self
    }

    /// Supply the agent routing used to scope engagement overrides: the
    /// channel/workspace bindings and the registry default agent.
    pub fn with_agent_scope(
        mut self,
        agent_bindings: HashMap<String, String>,
        default_agent_id: Option<String>,
    ) -> Self {
        self.agent_bindings = agent_bindings;
        self.default_agent_id = default_agent_id;
        self
    }

    /// Attach the channel-monitor event tap.
    pub fn with_monitor_tx(mut self, tx: mpsc::Sender<InboundEvent>) -> Self {
        self.monitor_tx = Some(tx);
        self
    }

    /// Attach the channel persona override layer (`persona` monitors).
    pub fn with_persona_overrides(mut self, personas: Arc<PersonaOverrides>) -> Self {
        self.persona_overrides = Some(personas);
        self
    }

    /// Fan a posted message out to the monitor engine. Fires before the chat-loop
    /// self/bot/should-respond gates so monitors observe every message (including
    /// other apps' automated posts); the engine's loop guard drops our own output.
    /// Edit/delete subtypes are skipped — monitors act on posted messages.
    fn emit_monitor_event(&self, event: &SlackEvent) {
        let Some(tx) = self.monitor_tx.as_ref() else {
            return;
        };
        if !matches!(event.event_type.as_str(), "message" | "app_mention") {
            return;
        }
        if event
            .subtype
            .as_deref()
            .is_some_and(|s| matches!(s, "message_changed" | "message_deleted"))
        {
            return;
        }
        let (Some(channel), Some(ts)) = (event.channel.clone(), event.ts.clone()) else {
            return;
        };
        let is_bot = event.bot_id.is_some() || event.subtype.as_deref() == Some("bot_message");
        let text = event_content(event, is_bot).finalized().into_text();
        let ev = InboundEvent {
            surface: "slack".to_string(),
            workspace: self.workspace_id.clone(),
            channel,
            user: event.user.clone(),
            bot_id: event.bot_id.clone(),
            text,
            ts,
            thread_ts: event.thread_ts.clone(),
            has_files: event.files.as_ref().is_some_and(|f| !f.is_empty()),
        };
        if let Err(e) = tx.try_send(ev) {
            debug!(error = %e, "monitor event tap full or closed; sweep will recover");
        }
    }

    /// Process a raw Slack event payload and return an action.
    pub async fn handle(&self, payload: Value) -> SlackAction {
        if let Some(tag) = payload.get("_driftwerk_type").and_then(|v| v.as_str()) {
            match tag {
                "slash_command" => return self.handle_slash_command(&payload),
                "interactive:block_actions" => return self.handle_block_actions(&payload),
                t if t.starts_with("interactive:") => return SlackAction::Ignore,
                _ => {}
            }
        }

        let event = match payload.get("event") {
            Some(e) => e,
            None => return SlackAction::Ignore,
        };

        // Log presence of files in raw JSON before parsing
        let raw_has_files = event
            .get("files")
            .and_then(|f| f.as_array())
            .is_some_and(|a| !a.is_empty());
        if raw_has_files {
            tracing::info!(
                raw_files = ?event.get("files"),
                "Raw Slack event contains files"
            );
        }

        let event: SlackEvent = match serde_json::from_value(event.clone()) {
            Ok(e) => e,
            Err(e) => {
                warn!("Failed to parse event: {}", e);
                return SlackAction::Ignore;
            }
        };

        // Independent monitor tap, before the chat-loop gates below.
        self.emit_monitor_event(&event);

        // Skip bot messages and self-messages (except for subtypes like message_changed/deleted)
        let is_meta_subtype = event
            .subtype
            .as_deref()
            .is_some_and(|s| matches!(s, "message_changed" | "message_deleted"));

        if !is_meta_subtype {
            if event.bot_id.is_some() {
                return SlackAction::Ignore;
            }
            if event.user.as_deref() == Some(&self.bot_user_id) {
                return SlackAction::Ignore;
            }
        }

        match event.event_type.as_str() {
            "message" | "app_mention" => {
                // Check for message subtypes first
                match event.subtype.as_deref() {
                    Some("message_changed") => self.handle_message_changed(event),
                    Some("message_deleted") => self.handle_message_deleted(event),
                    _ => self.handle_message(event).await,
                }
            }
            "reaction_added" => self.handle_reaction(event),
            "reaction_removed" => self.handle_reaction_removed(event),
            "member_joined_channel" => self.handle_member_joined(event),
            "member_left_channel" => self.handle_member_left(event),
            _ => {
                debug!(event_type = %event.event_type, "Ignoring event type");
                SlackAction::Ignore
            }
        }
    }

    async fn handle_message(&self, event: SlackEvent) -> SlackAction {
        let has_files = event.files.as_ref().is_some_and(|f| !f.is_empty());
        let file_count = event.files.as_ref().map(|f| f.len()).unwrap_or(0);
        if has_files {
            tracing::info!(files = file_count, "Message has file attachments");
        }
        let text = event.text.clone().unwrap_or_default();
        let channel = match event.channel {
            Some(ref c) => c.clone(),
            None => return SlackAction::Ignore,
        };
        let user = event.user.clone().unwrap_or_else(|| "unknown".to_string());
        let is_dm = event.channel_type.as_deref() == Some("im");

        // Channel allowlist check
        if !self.channel_allowlist.is_empty() && !self.channel_allowlist.contains(&channel) {
            debug!(channel = %channel, "Ignoring message from non-allowed channel");
            return SlackAction::Ignore;
        }

        // Resolve per-channel overrides
        let channel_override = self.channel_overrides.get(&channel);

        // A `persona` monitor's derived, hot prompt + mode for this channel.
        let persona = match self.persona_overrides {
            Some(ref po) => po.resolve(&channel).await,
            None => None,
        };

        // A persona binding with `exclude_thread_replies` keeps the bound agent
        // out of threads: it converses on top-level channel messages only, so a
        // reply inside a thread is ignored — even a direct @mention there.
        // Mirrors `InboundEvent::is_thread_reply` (thread_ts present and ≠ ts);
        // the `zip` makes a ts-less event fall through to "not a reply" so the
        // fail-safe direction is to respond, never to go silently quiet.
        if persona.as_ref().is_some_and(|p| p.exclude_thread_replies)
            && event
                .thread_ts
                .as_deref()
                .zip(event.ts.as_deref())
                .is_some_and(|(thread, ts)| thread != ts)
        {
            debug!(channel = %channel, "Skipping thread reply (persona exclude_thread_replies)");
            return SlackAction::Ignore;
        }

        // Precedence: a user's runtime set_mode > a persona > static config > default.
        // The override is scoped to the agent that serves this channel, so one
        // bot's mode never leaks to another sharing the channel.
        let runtime_mode = match (
            self.engagement_overrides.as_ref(),
            serving_agent(
                &self.agent_bindings,
                &channel,
                &self.workspace_id,
                self.default_agent_id.as_deref(),
            ),
        ) {
            (Some(eo), Some(agent_id)) => {
                eo.resolve(&agent_id, &channel, event.thread_ts.as_deref())
                    .await
            }
            _ => None,
        };

        let effective_response_mode = runtime_mode
            .or_else(|| persona.as_ref().map(|p| p.mode.clone()))
            .unwrap_or_else(|| {
                channel_override
                    .and_then(|o| o.response_mode.clone())
                    .unwrap_or_else(|| self.response_mode.clone())
            });

        // Use per-channel activation keywords if configured, else global
        let effective_keywords = channel_override
            .and_then(|o| o.activation_keywords.as_ref())
            .unwrap_or(&self.activation_keywords);

        let is_mentioned = self.message_mentions_bot(&text) || event.event_type == "app_mention";

        if !Self::should_respond_with(
            &effective_response_mode,
            effective_keywords,
            is_mentioned,
            is_dm,
            &text,
        ) {
            debug!(
                channel = %channel,
                response_mode = ?effective_response_mode,
                "Skipping message (activation gating)"
            );
            return SlackAction::Ignore;
        }

        // Composed from the unstripped text: whether the `rich_text` block
        // mirrors `text` must not depend on the mention being removed.
        let clean_text = self.strip_self_mention(&text);
        let content = event_content(&event, false)
            .map_body(|body| self.strip_self_mention(body))
            .finalized();
        if content.is_empty() && !has_files {
            return SlackAction::Ignore;
        }

        if let Some(ts) = event.ts.as_deref() {
            if self.is_duplicate(&channel, ts).await {
                debug!(channel = %channel, ts = %ts, "Dropping duplicate message (message/app_mention pair)");
                return SlackAction::Ignore;
            }
        }

        let session_key = if self.thread_isolation {
            if let Some(ref thread_ts) = event.thread_ts {
                format!("slack:{}:{}:{}", self.workspace_id, channel, thread_ts)
            } else if let Some(ref ts) = event.ts {
                format!("slack:{}:{}:{}", self.workspace_id, channel, ts)
            } else {
                format!("slack:{}:{}", self.workspace_id, channel)
            }
        } else {
            format!("slack:{}:{}", self.workspace_id, channel)
        };

        // Attach per-channel system prompt (a persona's prompt wins over static
        // config) and display name.
        let system_prompt = persona
            .as_ref()
            .map(|p| p.system_prompt.clone())
            .or_else(|| channel_override.and_then(|o| o.system_prompt.clone()));
        let channel_display_name = channel_override.and_then(|o| o.display_name.clone());

        SlackAction::ProcessMessage {
            workspace_id: self.workspace_id.clone(),
            session_key,
            user,
            raw_text: text,
            text: clean_text,
            body: content.body,
            shared: content.shared,
            channel,
            thread_ts: event.thread_ts,
            message_ts: event.ts,
            is_dm,
            is_mentioned,
            system_prompt,
            channel_display_name,
            files: event.files.unwrap_or_default(),
        }
    }

    fn handle_reaction(&self, event: SlackEvent) -> SlackAction {
        let user = match event.user {
            Some(u) if u != self.bot_user_id => u,
            _ => return SlackAction::Ignore,
        };
        let reaction = match event.reaction {
            Some(r) => r,
            None => return SlackAction::Ignore,
        };
        let (channel, ts) = match event.item {
            Some(ref item) if item.item_type == "message" => {
                match (item.channel.as_deref(), item.ts.as_deref()) {
                    (Some(ch), Some(ts)) => (ch.to_string(), ts.to_string()),
                    _ => return SlackAction::Ignore,
                }
            }
            _ => return SlackAction::Ignore,
        };

        SlackAction::ReactionAdded {
            workspace_id: self.workspace_id.clone(),
            user,
            channel,
            message_ts: ts,
            reaction,
        }
    }

    fn handle_reaction_removed(&self, event: SlackEvent) -> SlackAction {
        let user = match event.user {
            Some(u) if u != self.bot_user_id => u,
            _ => return SlackAction::Ignore,
        };
        let reaction = match event.reaction {
            Some(r) => r,
            None => return SlackAction::Ignore,
        };
        let (channel, ts) = match event.item {
            Some(ref item) if item.item_type == "message" => {
                match (item.channel.as_deref(), item.ts.as_deref()) {
                    (Some(ch), Some(ts)) => (ch.to_string(), ts.to_string()),
                    _ => return SlackAction::Ignore,
                }
            }
            _ => return SlackAction::Ignore,
        };

        SlackAction::ReactionRemoved {
            workspace_id: self.workspace_id.clone(),
            user,
            channel,
            message_ts: ts,
            reaction,
        }
    }

    fn handle_message_changed(&self, event: SlackEvent) -> SlackAction {
        let channel = match event.channel {
            Some(ref c) => c.clone(),
            None => return SlackAction::Ignore,
        };

        let new_msg = match event.message {
            Some(m) => m,
            None => return SlackAction::Ignore,
        };
        let old_msg = match event.previous_message {
            Some(m) => m,
            None => return SlackAction::Ignore,
        };

        let user = new_msg.user.unwrap_or_else(|| "unknown".to_string());
        let new_text = new_msg.text.unwrap_or_default();
        let old_text = old_msg.text.unwrap_or_default();
        let message_ts = new_msg.ts.unwrap_or_default();

        // Skip if the text didn't actually change
        if new_text == old_text {
            return SlackAction::Ignore;
        }

        SlackAction::MessageChanged {
            workspace_id: self.workspace_id.clone(),
            channel,
            user,
            old_text,
            new_text,
            message_ts,
        }
    }

    fn handle_message_deleted(&self, event: SlackEvent) -> SlackAction {
        let channel = match event.channel {
            Some(ref c) => c.clone(),
            None => return SlackAction::Ignore,
        };
        let deleted_ts = match event.deleted_ts {
            Some(ts) => ts,
            None => return SlackAction::Ignore,
        };

        SlackAction::MessageDeleted {
            workspace_id: self.workspace_id.clone(),
            channel,
            deleted_ts,
        }
    }

    fn handle_member_joined(&self, event: SlackEvent) -> SlackAction {
        let user = match event.user {
            Some(u) => u,
            None => return SlackAction::Ignore,
        };
        let channel = match event.channel {
            Some(c) => c,
            None => return SlackAction::Ignore,
        };

        SlackAction::MemberJoinedChannel {
            workspace_id: self.workspace_id.clone(),
            user,
            channel,
        }
    }

    fn handle_member_left(&self, event: SlackEvent) -> SlackAction {
        let user = match event.user {
            Some(u) => u,
            None => return SlackAction::Ignore,
        };
        let channel = match event.channel {
            Some(c) => c,
            None => return SlackAction::Ignore,
        };

        SlackAction::MemberLeftChannel {
            workspace_id: self.workspace_id.clone(),
            user,
            channel,
        }
    }

    fn handle_block_actions(&self, payload: &Value) -> SlackAction {
        let user = payload["user"]["id"].as_str().unwrap_or("").to_string();
        let user_team = payload["user"]["team_id"]
            .as_str()
            .filter(|team| !team.is_empty())
            .map(str::to_string);
        let channel = payload["channel"]["id"].as_str().unwrap_or("").to_string();
        let message_ts = payload["message"]["ts"].as_str().map(|s| s.to_string());
        let thread_ts = payload["message"]["thread_ts"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| message_ts.clone());
        let response_url = payload["response_url"].as_str().unwrap_or("").to_string();

        let actions = match payload.get("actions").and_then(|a| a.as_array()) {
            Some(a) if !a.is_empty() => a,
            _ => return SlackAction::Ignore,
        };
        // Slack sends an array but the convention is one action per payload.
        let a = &actions[0];
        let action_id = match a.get("action_id").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return SlackAction::Ignore,
        };
        let value = a.get("value").and_then(|v| v.as_str()).map(String::from);
        let message_blocks = payload
            .get("message")
            .and_then(|m| m.get("blocks"))
            .filter(|b| b.is_array())
            .cloned();

        if user.is_empty() || channel.is_empty() {
            return SlackAction::Ignore;
        }

        SlackAction::InteractiveAction {
            workspace_id: self.workspace_id.clone(),
            user,
            user_team,
            channel,
            thread_ts,
            message_ts,
            action_id,
            value,
            message_blocks,
            response_url,
        }
    }

    fn handle_slash_command(&self, payload: &Value) -> SlackAction {
        let command = payload["command"].as_str().unwrap_or("").to_string();
        let text = payload["text"].as_str().unwrap_or("").to_string();
        let user = payload["user_id"].as_str().unwrap_or("unknown").to_string();
        let channel = payload["channel_id"].as_str().unwrap_or("").to_string();
        let response_url = payload["response_url"].as_str().unwrap_or("").to_string();

        if command.is_empty() || channel.is_empty() {
            return SlackAction::Ignore;
        }

        let session_key = format!("slack:{}:{}:slash", self.workspace_id, channel);
        let is_dm = ConversationVisibility::from_slack_channel_id(&channel).is_private();

        SlackAction::SlashCommand {
            workspace_id: self.workspace_id.clone(),
            session_key,
            user,
            command,
            text,
            channel,
            response_url,
            is_dm,
        }
    }

    fn message_mentions_bot(&self, text: &str) -> bool {
        self.self_mention
            .as_ref()
            .is_some_and(|mention| mention.is_match(text))
    }

    fn strip_self_mention(&self, text: &str) -> String {
        self.self_mention
            .as_ref()
            .map_or(std::borrow::Cow::Borrowed(text), |mention| {
                mention.replace_all(text, "")
            })
            .trim()
            .to_string()
    }

    #[cfg(test)]
    fn should_respond(&self, is_mentioned: bool, is_dm: bool, text: &str) -> bool {
        Self::should_respond_with(
            &self.response_mode,
            &self.activation_keywords,
            is_mentioned,
            is_dm,
            text,
        )
    }

    fn should_respond_with(
        response_mode: &ResponseMode,
        activation_keywords: &[String],
        is_mentioned: bool,
        is_dm: bool,
        text: &str,
    ) -> bool {
        if is_dm {
            return true;
        }

        if is_mentioned {
            return true;
        }

        match response_mode {
            ResponseMode::All => true,
            ResponseMode::Quiet => false,
            ResponseMode::MentionOnly => activation_keywords
                .iter()
                .any(|kw| text.to_lowercase().contains(&kw.to_lowercase())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_handler() -> SlackEventHandler {
        SlackEventHandler::new(
            "test".to_string(),
            "UBOT".to_string(),
            ResponseMode::MentionOnly,
            vec![],
            false,
            vec![],
        )
    }

    fn seen(cap: usize) -> LruCache<String, ()> {
        LruCache::new(NonZeroUsize::new(cap).unwrap())
    }

    #[tokio::test]
    async fn block_actions_carry_the_clickers_team() {
        let handler = make_handler();
        let click = |user: serde_json::Value| {
            serde_json::json!({
                "_driftwerk_type": "interactive:block_actions",
                "user": user,
                "team": { "id": "T_HOME" },
                "channel": { "id": "C1" },
                "message": { "ts": "1.0" },
                "response_url": "https://hooks.example.com/r",
                "actions": [{ "action_id": "wf_decision_0", "value": "run|gate|send" }],
            })
        };
        let team_of = |action: SlackAction| match action {
            SlackAction::InteractiveAction { user_team, .. } => user_team,
            other => panic!("not an interactive action: {other:?}"),
        };

        let guest = handler
            .handle(click(
                serde_json::json!({ "id": "U1", "team_id": "T_GUEST" }),
            ))
            .await;
        let member = handler
            .handle(click(serde_json::json!({ "id": "U2" })))
            .await;

        assert_eq!(team_of(guest).as_deref(), Some("T_GUEST"));
        assert_eq!(team_of(member), None);
    }

    #[test]
    fn record_seen_reports_repeat() {
        let mut s = seen(4);
        assert!(!record_seen(&mut s, "a".to_string()));
        assert!(record_seen(&mut s, "a".to_string()));
        assert!(!record_seen(&mut s, "b".to_string()));
    }

    #[test]
    fn record_seen_evicts_oldest() {
        let mut s = seen(1);
        assert!(!record_seen(&mut s, "a".to_string()));
        assert!(!record_seen(&mut s, "b".to_string()));
        assert!(!record_seen(&mut s, "a".to_string()));
    }

    #[test]
    fn should_respond_dm() {
        let handler = make_handler();
        assert!(handler.should_respond(false, true, "hello"));
    }

    #[test]
    fn should_respond_mention() {
        let handler = make_handler();
        assert!(handler.should_respond(true, false, "hello"));
    }

    #[test]
    fn should_not_respond_unmentioned_in_shared_channel() {
        let handler = make_handler();
        assert!(!handler.should_respond(false, false, "follow up in some thread"));
    }

    #[test]
    fn shared_channel_thread_continuation_does_not_trigger() {
        let handler = make_handler();
        assert!(!handler.should_respond(false, false, "thread reply with no mention"));
    }

    #[test]
    fn should_respond_all_mode() {
        let handler = SlackEventHandler::new(
            "test".to_string(),
            "UBOT".to_string(),
            ResponseMode::All,
            vec![],
            false,
            vec![],
        );
        assert!(handler.should_respond(false, false, "some casual message"));
    }

    #[test]
    fn should_respond_keyword() {
        let handler = SlackEventHandler::new(
            "test".to_string(),
            "UBOT".to_string(),
            ResponseMode::MentionOnly,
            vec!["help".to_string()],
            false,
            vec![],
        );
        assert!(handler.should_respond(false, false, "I need help please"));
        assert!(!handler.should_respond(false, false, "just chatting"));
    }

    #[test]
    fn quiet_mode_only_responds_to_dm_or_mention() {
        let handler = SlackEventHandler::new(
            "test".to_string(),
            "UBOT".to_string(),
            ResponseMode::Quiet,
            vec![],
            false,
            vec![],
        );
        assert!(!handler.should_respond(false, false, "hello"));
        assert!(handler.should_respond(true, false, "mentioned"));
        assert!(handler.should_respond(false, true, "dm"));
    }

    #[test]
    fn message_mentions_bot_detection() {
        let handler = make_handler();
        assert!(handler.message_mentions_bot("<@UBOT> hello"));
        assert!(!handler.message_mentions_bot("hello"));
        assert!(!handler.message_mentions_bot("<@UOTHER> hello"));
    }

    #[tokio::test]
    async fn handle_ignores_bot_messages() {
        let handler = make_handler();
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "bot_id": "B123",
                "text": "bot message",
                "channel": "C123",
                "ts": "100.001"
            }
        });
        assert!(matches!(handler.handle(payload).await, SlackAction::Ignore));
    }

    #[tokio::test]
    async fn monitor_tap_composes_bot_attachment_content() {
        let (tx, mut rx) = mpsc::channel(4);
        let handler = make_handler().with_monitor_tx(tx);
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "bot_id": "B_DATADOG",
                "text": "",
                "channel": "C123",
                "ts": "100.001",
                "attachments": [{
                    "title": "Recovered: Consecutive Failures detected",
                    "text": "Alert has recovered",
                }],
            }
        });

        // The chat loop still ignores the bot post…
        assert!(matches!(handler.handle(payload).await, SlackAction::Ignore));
        // …but the monitor tap sees the alert body, not an empty string.
        let ev = rx.try_recv().expect("monitor tap should receive the event");
        assert_eq!(ev.bot_id.as_deref(), Some("B_DATADOG"));
        assert_eq!(
            ev.text,
            "Recovered: Consecutive Failures detected\nAlert has recovered"
        );
    }

    #[tokio::test]
    async fn handle_ignores_self_messages() {
        let handler = make_handler();
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "UBOT",
                "text": "my own message",
                "channel": "C123",
                "ts": "100.001"
            }
        });
        assert!(matches!(handler.handle(payload).await, SlackAction::Ignore));
    }

    #[tokio::test]
    async fn handle_app_mention_produces_action() {
        let handler = make_handler();
        let payload = serde_json::json!({
            "event": {
                "type": "app_mention",
                "user": "U_USER",
                "text": "<@UBOT> do something",
                "channel": "C123",
                "ts": "100.001"
            }
        });
        match handler.handle(payload).await {
            SlackAction::ProcessMessage {
                text, is_mentioned, ..
            } => {
                assert!(is_mentioned);
                assert_eq!(text, "do something");
            }
            other => panic!("Expected ProcessMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn persona_exclude_thread_replies_ignores_in_thread_keeps_top_level_and_root() {
        use workforce_common::persona::ChannelPersona;

        let personas = Arc::new(PersonaOverrides::new());
        let mut map = std::collections::HashMap::new();
        map.insert(
            "C123".to_string(),
            ChannelPersona {
                system_prompt: "be helpful".to_string(),
                // `All` would otherwise answer everything — proves the gate runs
                // before mode evaluation.
                mode: ResponseMode::All,
                agent: "lynx".to_string(),
                monitor_id: "m".to_string(),
                exclude_thread_replies: true,
            },
        );
        personas.replace_all(map).await;
        let handler = make_handler().with_persona_overrides(personas);

        // A reply inside a thread (thread_ts present, ≠ ts) → ignored.
        let reply = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "in-thread chatter",
                "channel": "C123",
                "ts": "200.002",
                "thread_ts": "100.001"
            }
        });
        assert!(matches!(handler.handle(reply).await, SlackAction::Ignore));

        // A top-level message (no thread) → still processed under persona All mode.
        let top = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "channel-level message",
                "channel": "C123",
                "ts": "300.003"
            }
        });
        assert!(matches!(
            handler.handle(top).await,
            SlackAction::ProcessMessage { .. }
        ));

        // The thread root (thread_ts == ts) → still processed.
        let root = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "thread root",
                "channel": "C123",
                "ts": "400.004",
                "thread_ts": "400.004"
            }
        });
        assert!(matches!(
            handler.handle(root).await,
            SlackAction::ProcessMessage { .. }
        ));
    }

    fn channel_mention(event_type: &str, ts: &str) -> Value {
        serde_json::json!({
            "event": {
                "type": event_type,
                "user": "U_USER",
                "text": "<@UBOT> do something",
                "channel": "C123",
                "ts": ts
            }
        })
    }

    #[tokio::test]
    async fn handle_dedups_message_app_mention_pair() {
        let handler = make_handler();
        assert!(matches!(
            handler
                .handle(channel_mention("app_mention", "100.001"))
                .await,
            SlackAction::ProcessMessage { .. }
        ));
        assert!(matches!(
            handler.handle(channel_mention("message", "100.001")).await,
            SlackAction::Ignore
        ));
    }

    #[tokio::test]
    async fn handle_dedups_pair_regardless_of_order() {
        let handler = make_handler();
        assert!(matches!(
            handler.handle(channel_mention("message", "200.002")).await,
            SlackAction::ProcessMessage { .. }
        ));
        assert!(matches!(
            handler
                .handle(channel_mention("app_mention", "200.002"))
                .await,
            SlackAction::Ignore
        ));
    }

    #[tokio::test]
    async fn handle_does_not_dedup_distinct_messages() {
        let handler = make_handler();
        assert!(matches!(
            handler
                .handle(channel_mention("app_mention", "300.001"))
                .await,
            SlackAction::ProcessMessage { .. }
        ));
        assert!(matches!(
            handler
                .handle(channel_mention("app_mention", "300.002"))
                .await,
            SlackAction::ProcessMessage { .. }
        ));
    }

    #[tokio::test]
    async fn handle_dm_produces_action() {
        let handler = make_handler();
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "hello in DM",
                "channel": "D123",
                "channel_type": "im",
                "ts": "100.001"
            }
        });
        match handler.handle(payload).await {
            SlackAction::ProcessMessage { is_dm, text, .. } => {
                assert!(is_dm);
                assert_eq!(text, "hello in DM");
            }
            other => panic!("Expected ProcessMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_reaction_added() {
        let handler = make_handler();
        let payload = serde_json::json!({
            "event": {
                "type": "reaction_added",
                "user": "U_USER",
                "reaction": "thumbsup",
                "item": {
                    "type": "message",
                    "channel": "C123",
                    "ts": "100.001"
                }
            }
        });
        match handler.handle(payload).await {
            SlackAction::ReactionAdded {
                user,
                channel,
                message_ts,
                reaction,
                ..
            } => {
                assert_eq!(user, "U_USER");
                assert_eq!(channel, "C123");
                assert_eq!(message_ts, "100.001");
                assert_eq!(reaction, "thumbsup");
            }
            other => panic!("Expected ReactionAdded, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_ignores_missing_event() {
        let handler = make_handler();
        let payload = serde_json::json!({"type": "hello"});
        assert!(matches!(handler.handle(payload).await, SlackAction::Ignore));
    }

    #[tokio::test]
    async fn handle_ignores_empty_text() {
        let handler = make_handler();
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "",
                "channel": "D123",
                "channel_type": "im",
                "ts": "100.001"
            }
        });
        assert!(matches!(handler.handle(payload).await, SlackAction::Ignore));
    }

    #[tokio::test]
    async fn handle_channel_allowlist_filters() {
        let handler = SlackEventHandler::new(
            "test".to_string(),
            "UBOT".to_string(),
            ResponseMode::All,
            vec![],
            false,
            vec!["C_ALLOWED".to_string()],
        );
        // Message from non-allowed channel → ignored
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "hello",
                "channel": "C_OTHER",
                "ts": "100.001"
            }
        });
        assert!(matches!(handler.handle(payload).await, SlackAction::Ignore));

        // Message from allowed channel → processed
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "hello",
                "channel": "C_ALLOWED",
                "ts": "100.002"
            }
        });
        assert!(matches!(
            handler.handle(payload).await,
            SlackAction::ProcessMessage { .. }
        ));
    }

    #[tokio::test]
    async fn handle_thread_isolation_session_key() {
        let handler = SlackEventHandler::new(
            "test".to_string(),
            "UBOT".to_string(),
            ResponseMode::All,
            vec![],
            true, // thread_isolation ON
            vec![],
        );
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "threaded msg",
                "channel": "C123",
                "ts": "200.002",
                "thread_ts": "100.001"
            }
        });
        match handler.handle(payload).await {
            SlackAction::ProcessMessage { session_key, .. } => {
                assert_eq!(session_key, "slack:test:C123:100.001");
            }
            other => panic!("Expected ProcessMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn channel_override_response_mode() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "C_ALL".to_string(),
            SlackChannelOverride {
                response_mode: Some(ResponseMode::All),
                ..Default::default()
            },
        );
        let handler = SlackEventHandler::new(
            "test".to_string(),
            "UBOT".to_string(),
            ResponseMode::MentionOnly, // default: mention only
            vec![],
            false,
            vec![],
        )
        .with_channel_overrides(overrides);

        // C_ALL channel should respond to all messages (overridden)
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "hello",
                "channel": "C_ALL",
                "ts": "100.001"
            }
        });
        assert!(matches!(
            handler.handle(payload).await,
            SlackAction::ProcessMessage { .. }
        ));

        // Other channels still use global MentionOnly → ignored
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "hello",
                "channel": "C_OTHER",
                "ts": "100.002"
            }
        });
        assert!(matches!(handler.handle(payload).await, SlackAction::Ignore));
    }

    #[tokio::test]
    async fn channel_override_system_prompt() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "C_DEV".to_string(),
            SlackChannelOverride {
                system_prompt: Some("This is #dev — focused on development.".to_string()),
                display_name: Some("#dev".to_string()),
                ..Default::default()
            },
        );
        let handler = SlackEventHandler::new(
            "test".to_string(),
            "UBOT".to_string(),
            ResponseMode::All,
            vec![],
            false,
            vec![],
        )
        .with_channel_overrides(overrides);

        // Channel with override should have system_prompt and display_name
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "hello",
                "channel": "C_DEV",
                "ts": "100.001"
            }
        });
        match handler.handle(payload).await {
            SlackAction::ProcessMessage {
                system_prompt,
                channel_display_name,
                ..
            } => {
                assert_eq!(
                    system_prompt.as_deref(),
                    Some("This is #dev — focused on development.")
                );
                assert_eq!(channel_display_name.as_deref(), Some("#dev"));
            }
            other => panic!("Expected ProcessMessage, got {:?}", other),
        }

        // Channel without override should have no system_prompt
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "hello",
                "channel": "C_OTHER",
                "ts": "100.002"
            }
        });
        match handler.handle(payload).await {
            SlackAction::ProcessMessage {
                system_prompt,
                channel_display_name,
                ..
            } => {
                assert!(system_prompt.is_none());
                assert!(channel_display_name.is_none());
            }
            other => panic!("Expected ProcessMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn channel_override_activation_keywords() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "C_HELP".to_string(),
            SlackChannelOverride {
                activation_keywords: Some(vec!["deploy".to_string()]),
                ..Default::default()
            },
        );
        let handler = SlackEventHandler::new(
            "test".to_string(),
            "UBOT".to_string(),
            ResponseMode::MentionOnly,
            vec!["help".to_string()], // global keyword
            false,
            vec![],
        )
        .with_channel_overrides(overrides);

        // C_HELP: "deploy" triggers (per-channel keyword)
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "please deploy the app",
                "channel": "C_HELP",
                "ts": "100.001"
            }
        });
        assert!(matches!(
            handler.handle(payload).await,
            SlackAction::ProcessMessage { .. }
        ));

        // C_HELP: "help" does NOT trigger (global keyword overridden by per-channel)
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "I need help",
                "channel": "C_HELP",
                "ts": "100.002"
            }
        });
        assert!(matches!(handler.handle(payload).await, SlackAction::Ignore));

        // Other channel: "help" triggers (uses global keywords)
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "I need help",
                "channel": "C_OTHER",
                "ts": "100.003"
            }
        });
        assert!(matches!(
            handler.handle(payload).await,
            SlackAction::ProcessMessage { .. }
        ));
    }

    fn pasted_table_attachment() -> Value {
        serde_json::json!({
            "id": 1,
            "fallback": "[no preview available]",
            "blocks": [{"type": "table", "rows": [
                [{"type": "raw_text", "text": "Email"}, {"type": "raw_text", "text": "Status"}],
                [{"type": "raw_text", "text": "a.b@example.org"}, {"type": "raw_text", "text": "Failed"}],
            ]}],
        })
    }

    #[tokio::test]
    async fn live_message_with_pasted_table_carries_rows_in_body() {
        let handler = make_handler();
        let payload = serde_json::json!({
            "event": {
                "type": "app_mention",
                "user": "U_USER",
                "text": "<@UBOT> here they are again:",
                "blocks": mirror_block(serde_json::json!([
                    {"type": "user", "user_id": "UBOT"},
                    {"type": "text", "text": " here they are again:"},
                ])),
                "channel": "C123",
                "ts": "300.001",
                "attachments": [pasted_table_attachment()],
            }
        });
        match handler.handle(payload).await {
            SlackAction::ProcessMessage { text, body, .. } => {
                assert_eq!(text, "here they are again:");
                assert_eq!(
                    body,
                    "here they are again:\n| Email | Status |\n| a.b@example.org | Failed |"
                );
            }
            other => panic!("Expected ProcessMessage, got {:?}", other),
        }
    }

    /// The `rich_text` block Slack's own clients attach to a typed message.
    fn mirror_block(elements: Value) -> Value {
        serde_json::json!([{"type": "rich_text", "elements": [
            {"type": "rich_text_section", "elements": elements},
        ]}])
    }

    #[tokio::test]
    async fn mention_plus_table_is_processed() {
        let handler = make_handler();
        let payload = serde_json::json!({
            "event": {
                "type": "app_mention",
                "user": "U_USER",
                "text": "<@UBOT>",
                "blocks": mirror_block(serde_json::json!([{"type": "user", "user_id": "UBOT"}])),
                "channel": "C123",
                "ts": "300.002",
                "attachments": [pasted_table_attachment()],
            }
        });
        match handler.handle(payload).await {
            SlackAction::ProcessMessage { text, body, .. } => {
                assert_eq!(text, "");
                assert_eq!(body, "| Email | Status |\n| a.b@example.org | Failed |");
            }
            other => panic!("Expected ProcessMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn block_only_human_message_is_not_ignored() {
        let handler = make_handler();
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "",
                "channel": "D123",
                "channel_type": "im",
                "ts": "300.003",
                "attachments": [{
                    "is_share": true,
                    "author_name": "Alerts",
                    "text": "CPU high on host-1",
                }],
            }
        });
        match handler.handle(payload).await {
            SlackAction::ProcessMessage { body, shared, .. } => {
                assert_eq!(body, "");
                assert_eq!(
                    shared,
                    ["[shared message from Alerts]\n> CPU high on host-1"]
                );
            }
            other => panic!("Expected ProcessMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn table_only_dm_is_processed_and_shows_channel_with_id() {
        let handler = make_handler();
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "",
                "channel": "D123",
                "channel_type": "im",
                "ts": "300.009",
                "blocks": [
                    {"type": "table", "rows": [[{"type": "raw_text", "text": "a < b"}]]},
                    {"type": "section", "text": {"type": "mrkdwn", "text": "post in <#C9|ops>"}},
                ],
            }
        });
        match handler.handle(payload).await {
            SlackAction::ProcessMessage { text, body, .. } => {
                assert_eq!(text, "");
                assert_eq!(body, "| a < b |\npost in #ops (C9)");
            }
            other => panic!("Expected ProcessMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn monitor_tap_carries_pasted_table_and_keeps_typed_tokens_inert() {
        let (tx, mut rx) = mpsc::channel(4);
        let handler = make_handler().with_monitor_tx(tx);
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "literally &lt;!channel&gt; R&amp;D",
                "channel": "C123",
                "ts": "300.010",
                "attachments": [pasted_table_attachment()],
            }
        });
        handler.handle(payload).await;
        assert_eq!(
            rx.try_recv().unwrap().text,
            "literally &lt;!channel> R&D\n| Email | Status |\n| a.b@example.org | Failed |"
        );
    }

    #[tokio::test]
    async fn bare_bot_mention_is_ignored() {
        let handler = make_handler();
        for (text, ts) in [("<@UBOT>", "300.004"), (" <@UBOT|helper> ", "300.005")] {
            let payload = serde_json::json!({
                "event": {
                    "type": "app_mention",
                    "user": "U_USER",
                    "text": text,
                    "blocks": mirror_block(serde_json::json!([{"type": "user", "user_id": "UBOT"}])),
                    "channel": "C123",
                    "ts": ts,
                }
            });
            assert!(
                matches!(handler.handle(payload).await, SlackAction::Ignore),
                "{text:?} must not start a turn"
            );
        }
    }

    #[tokio::test]
    async fn labelled_self_mention_is_detected_and_stripped() {
        let handler = make_handler();
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "hey <@UBOT|helper> ping <@UOTHER>",
                "channel": "C123",
                "ts": "300.006",
            }
        });
        match handler.handle(payload).await {
            SlackAction::ProcessMessage {
                text,
                body,
                is_mentioned,
                ..
            } => {
                assert!(is_mentioned);
                assert_eq!(text, "hey  ping <@UOTHER>");
                assert_eq!(body, "hey  ping <@UOTHER>");
            }
            other => panic!("Expected ProcessMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn live_body_matches_read_path_rendering() {
        let handler = make_handler();
        let message = serde_json::json!({
            "type": "message",
            "user": "U_USER",
            "text": "see <https://example.com/doc|the doc> for a &lt; b",
            "channel": "D123",
            "channel_type": "im",
            "ts": "300.007",
            "attachments": [pasted_table_attachment()],
        });
        let none = HashMap::new();
        let read = super::super::formatting::finalize_inbound_text(
            &super::super::inbound::compose_message_text(&message),
            &none,
            &none,
        );
        match handler
            .handle(serde_json::json!({ "event": message }))
            .await
        {
            SlackAction::ProcessMessage { body, .. } => {
                assert_eq!(body, read);
                assert!(body.starts_with("see the doc (https://example.com/doc) for a < b\n"));
            }
            other => panic!("Expected ProcessMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn file_entry_missing_fields_does_not_drop_event() {
        let handler = make_handler();
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "the report",
                "channel": "D123",
                "channel_type": "im",
                "ts": "300.008",
                "files": [
                    {"id": "F1", "mode": "hidden_by_limit"},
                    {"id": "F2", "title": "Q3 report", "size": "not-a-number"},
                    {"id": "F3", "title": "Q3 report", "mimetype": null},
                ],
            }
        });
        match handler.handle(payload).await {
            SlackAction::ProcessMessage { text, files, .. } => {
                assert_eq!(text, "the report");
                let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
                assert_eq!(names, ["file", "Q3 report", "Q3 report"]);
                assert!(files.iter().all(|f| f.url_private_download.is_none()));
            }
            other => panic!("Expected ProcessMessage, got {:?}", other),
        }
    }

    #[test]
    fn slack_file_deserialisation() {
        let json = serde_json::json!({
            "id": "F123",
            "name": "screenshot.png",
            "mimetype": "image/png",
            "size": 12345,
            "url_private_download": "https://files.slack.com/files-pri/T123-F123/download/screenshot.png"
        });
        let file: SlackFile = serde_json::from_value(json).unwrap();
        assert_eq!(file.id, "F123");
        assert_eq!(file.mimetype, "image/png");
        assert!(file.url_private_download.is_some());
    }

    #[test]
    fn slack_event_with_files_parses() {
        let json = serde_json::json!({
            "type": "message",
            "user": "U_USER",
            "text": "check this image",
            "channel": "C123",
            "ts": "100.001",
            "files": [{
                "id": "F123",
                "name": "photo.jpg",
                "mimetype": "image/jpeg",
                "size": 5000,
                "url_private_download": "https://files.slack.com/download/photo.jpg"
            }]
        });
        let event: SlackEvent = serde_json::from_value(json).unwrap();
        assert!(event.files.is_some());
        assert_eq!(event.files.as_ref().unwrap().len(), 1);
        assert_eq!(event.files.as_ref().unwrap()[0].name, "photo.jpg");
    }

    #[tokio::test]
    async fn handle_files_only_message_in_dm() {
        let handler = make_handler();
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "",
                "channel": "D123",
                "channel_type": "im",
                "ts": "100.001",
                "files": [{
                    "id": "F123",
                    "name": "image.png",
                    "mimetype": "image/png",
                    "size": 1000,
                    "url_private_download": "https://files.slack.com/download/image.png"
                }]
            }
        });
        match handler.handle(payload).await {
            SlackAction::ProcessMessage { files, text, .. } => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].name, "image.png");
                assert!(text.is_empty());
            }
            other => panic!("Expected ProcessMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_message_with_text_and_files() {
        let handler = make_handler();
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "<@UBOT> look at this",
                "channel": "C123",
                "ts": "100.001",
                "files": [{
                    "id": "F456",
                    "name": "diagram.png",
                    "mimetype": "image/png",
                    "size": 2000,
                    "url_private_download": "https://files.slack.com/download/diagram.png"
                }]
            }
        });
        match handler.handle(payload).await {
            SlackAction::ProcessMessage { files, text, .. } => {
                assert_eq!(files.len(), 1);
                assert_eq!(text, "look at this");
            }
            other => panic!("Expected ProcessMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_ignores_empty_text_no_files() {
        let handler = make_handler();
        // Empty text + no files → should still be ignored
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "",
                "channel": "D123",
                "channel_type": "im",
                "ts": "100.001"
            }
        });
        assert!(matches!(handler.handle(payload).await, SlackAction::Ignore));
    }

    #[tokio::test]
    async fn runtime_override_takes_precedence_over_config() {
        use workforce_common::engagement::{EngagementOverrides, EngagementScope};

        // Config says Quiet for C_TEST, but the agent's runtime override says All.
        let overrides = Arc::new(EngagementOverrides::new());
        overrides
            .set(
                EngagementScope::new("atlas", "C_TEST", None),
                ResponseMode::All,
                "agent",
            )
            .await;

        let mut config_overrides = HashMap::new();
        config_overrides.insert(
            "C_TEST".to_string(),
            SlackChannelOverride {
                response_mode: Some(ResponseMode::Quiet),
                ..Default::default()
            },
        );

        let handler = SlackEventHandler::new(
            "test".to_string(),
            "UBOT".to_string(),
            ResponseMode::MentionOnly,
            vec![],
            false,
            vec![],
        )
        .with_channel_overrides(config_overrides)
        .with_engagement_overrides(overrides)
        .with_agent_scope(HashMap::new(), Some("atlas".to_string()));

        // Without the runtime override, Quiet would suppress this message.
        // With the agent's runtime override set to All, it is processed.
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "hello",
                "channel": "C_TEST",
                "ts": "100.001"
            }
        });
        assert!(matches!(
            handler.handle(payload).await,
            SlackAction::ProcessMessage { .. }
        ));
    }

    #[tokio::test]
    async fn runtime_override_for_another_agent_does_not_leak() {
        use workforce_common::engagement::{EngagementOverrides, EngagementScope};

        // Bernard's agent set the channel to All; this handler serves Atlas,
        // whose config default is MentionOnly. A non-mention message must not
        // activate Atlas — the override belongs to a different agent.
        let overrides = Arc::new(EngagementOverrides::new());
        overrides
            .set(
                EngagementScope::new("bernard", "C_TEST", None),
                ResponseMode::All,
                "agent",
            )
            .await;

        let handler = SlackEventHandler::new(
            "test".to_string(),
            "UBOT".to_string(),
            ResponseMode::MentionOnly,
            vec![],
            false,
            vec![],
        )
        .with_engagement_overrides(overrides)
        .with_agent_scope(HashMap::new(), Some("atlas".to_string()));

        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "hello",
                "channel": "C_TEST",
                "ts": "100.001"
            }
        });
        assert!(matches!(handler.handle(payload).await, SlackAction::Ignore));
    }

    #[test]
    fn serving_agent_prefers_channel_then_workspace_then_default() {
        let mut bindings = HashMap::new();
        bindings.insert("ws".to_string(), "ws_agent".to_string());
        bindings.insert("C_SPECIAL".to_string(), "chan_agent".to_string());

        // Channel binding wins over workspace binding.
        assert_eq!(
            serving_agent(&bindings, "C_SPECIAL", "ws", Some("default")),
            Some("chan_agent".to_string())
        );
        // Otherwise the workspace binding applies.
        assert_eq!(
            serving_agent(&bindings, "C_OTHER", "ws", Some("default")),
            Some("ws_agent".to_string())
        );
        // Unbound workspace falls back to the registry default (single-agent case).
        assert_eq!(
            serving_agent(&bindings, "C_OTHER", "ws_none", Some("default")),
            Some("default".to_string())
        );
        // No binding and no default → no scope, so config defaults govern.
        assert_eq!(serving_agent(&bindings, "C_OTHER", "ws_none", None), None);
    }

    #[tokio::test]
    async fn no_runtime_override_falls_through_to_config() {
        // Config says All for C_TEST, no runtime override
        let mut config_overrides = HashMap::new();
        config_overrides.insert(
            "C_TEST".to_string(),
            SlackChannelOverride {
                response_mode: Some(ResponseMode::All),
                ..Default::default()
            },
        );

        let handler = SlackEventHandler::new(
            "test".to_string(),
            "UBOT".to_string(),
            ResponseMode::MentionOnly, // global default
            vec![],
            false,
            vec![],
        )
        .with_channel_overrides(config_overrides);

        // Config override All means respond to all — should process
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U_USER",
                "text": "hello",
                "channel": "C_TEST",
                "ts": "100.001"
            }
        });
        assert!(matches!(
            handler.handle(payload).await,
            SlackAction::ProcessMessage { .. }
        ));
    }

    fn test_handler() -> SlackEventHandler {
        SlackEventHandler::new(
            "W_TEST".to_string(),
            "U_BOT".to_string(),
            ResponseMode::MentionOnly,
            Vec::new(),
            false,
            Vec::new(),
        )
    }

    #[test]
    fn slash_command_flags_dm_by_channel_prefix() {
        let handler = test_handler();
        let dm_payload = serde_json::json!({
            "command": "/cc",
            "text": "driftwerk/workforce",
            "user_id": "U_USER",
            "channel_id": "D0000001",
            "response_url": "https://hooks.slack.com/commands/...",
        });
        match handler.handle_slash_command(&dm_payload) {
            SlackAction::SlashCommand { is_dm, .. } => assert!(is_dm),
            other => panic!("unexpected action: {other:?}"),
        }

        let channel_payload = serde_json::json!({
            "command": "/cc",
            "text": "driftwerk/workforce",
            "user_id": "U_USER",
            "channel_id": "C0000001",
            "response_url": "https://hooks.slack.com/commands/...",
        });
        match handler.handle_slash_command(&channel_payload) {
            SlackAction::SlashCommand { is_dm, .. } => assert!(!is_dm),
            other => panic!("unexpected action: {other:?}"),
        }
    }
}

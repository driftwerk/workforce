//! Self-contained Slack skill — socket mode transport, event handling, and tools.
//!
//! The Slack skill manages one or more workspace connections and provides:
//! - Socket mode transport for receiving events
//! - Event handling that produces [`SlackAction`] values for the orchestration layer
//! - Agent-callable tools: `slack.send`, `slack.react`, `slack.read`
//! - A [`ChannelSurface`] implementation for the generic messaging system

pub mod classifier;
pub mod config;
pub mod event_handler;
pub mod formatting;
pub mod inbound;
pub mod rich_text;
pub mod surface;
pub mod tools;
pub mod transport;

use std::sync::Arc;

use eyre::Result;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use workforce_agent::tools::Tool;
use workforce_common::channels::{ChannelSurface, InboundEvent};
use workforce_common::engagement::EngagementOverrides;
use workforce_common::persona::PersonaOverrides;

use crate::definition::{SkillDefinition, SkillEventSource, SkillPolicy, SkillTool};

pub use config::{SlackSkillConfig, SlackWorkspaceConfig, SlashCommandConfig, TeamChannelConfig};
pub use event_handler::SlackAction;
pub use surface::{SlackSurface, MAX_BLOCKS_PER_MESSAGE, MAX_SECTION_TEXT};
pub use transport::BotIdentity;

/// Result of starting the Slack skill, containing everything needed for
/// the orchestration layer and for clean teardown.
pub struct SlackStartResult {
    /// Trait-object surfaces for SurfaceRegistry registration.
    pub surfaces: Vec<Arc<dyn ChannelSurface>>,
    /// Concrete Slack surfaces for file downloads (fallback).
    pub concrete_surfaces: Vec<Arc<SlackSurface>>,
    /// Workspace ID → concrete Slack surface for file downloads.
    pub workspace_concrete_surfaces: std::collections::HashMap<String, Arc<SlackSurface>>,
    /// Workspace ID → surface mapping for targeted message dispatch.
    pub workspace_surfaces: std::collections::HashMap<String, Arc<dyn ChannelSurface>>,
    /// Workspace ID → bot identity (user_id + display name) resolved at
    /// startup via `auth.test`. The hub injects these into each bound
    /// agent's system prompt so the LLM recognises its own Slack user_id
    /// in inbound messages.
    pub workspace_bot_identities: std::collections::HashMap<String, BotIdentity>,
    /// Receiver for the action dispatch loop.
    pub action_rx: mpsc::Receiver<SlackAction>,
    /// Sender feeding `action_rx`, so the dispatch loop can re-inject synthetic
    /// actions (e.g. a playbook Continue button click becomes a `ProcessMessage`).
    pub action_tx: mpsc::Sender<SlackAction>,
    /// Agent binding config (channel/workspace → agent ID).
    pub bindings: std::collections::HashMap<String, String>,
    /// Team channel configurations for multi-identity routing.
    pub team_bindings: std::collections::HashMap<String, TeamChannelConfig>,
    /// JoinHandles for all spawned workspace tasks.
    /// Abort these to tear down the connection.
    pub task_handles: Vec<JoinHandle<()>>,
}

/// Outbound surfaces + bot identities + bindings built without opening a
/// socket. Produced by [`SlackSkill::build_surfaces`] and safe to register on
/// every hub instance (so any instance's turn can post to Slack); pair with
/// [`SlackSkill::start_transport`] on the leader to receive events.
pub struct SlackSurfaces {
    pub surfaces: Vec<Arc<dyn ChannelSurface>>,
    pub concrete_surfaces: Vec<Arc<SlackSurface>>,
    pub workspace_concrete_surfaces: std::collections::HashMap<String, Arc<SlackSurface>>,
    pub workspace_surfaces: std::collections::HashMap<String, Arc<dyn ChannelSurface>>,
    pub workspace_bot_identities: std::collections::HashMap<String, BotIdentity>,
    pub bindings: std::collections::HashMap<String, String>,
    pub team_bindings: std::collections::HashMap<String, TeamChannelConfig>,
    /// Workspaces left out because Slack refused their bot token, with
    /// Slack's reason. One revoked app never takes the others down.
    pub rejected: std::collections::BTreeMap<String, String>,
}

/// The action channel + task handles produced by opening the transport sockets
/// in [`SlackSkill::start_transport`]. Leader-only.
pub struct SlackTransportHandles {
    pub action_rx: mpsc::Receiver<SlackAction>,
    pub action_tx: mpsc::Sender<SlackAction>,
    pub task_handles: Vec<JoinHandle<()>>,
}

/// A running workspace connection.
struct WorkspaceRuntime {
    workspace_id: String,
    surface: Arc<SlackSurface>,
    event_handler: Arc<event_handler::SlackEventHandler>,
    config: SlackWorkspaceConfig,
}

/// The Slack skill — manages workspace connections and provides tools.
pub struct SlackSkill {
    config: SlackSkillConfig,
    runtimes: Vec<WorkspaceRuntime>,
    /// Per-user secret store used by user-scoped Slack endpoints
    /// (currently only `search.messages` via xoxp-… tokens). The bot
    /// identity is still workspace-shared.
    secret_store: Option<Arc<dyn workforce_common::settings::SecretStore>>,
    /// Channel-monitor live tap: every inbound message is forwarded here, and
    /// `on_connect` fires a catch-up sweep after each (re)connect.
    monitor_tx: Option<mpsc::Sender<InboundEvent>>,
    monitor_on_connect: Option<Arc<dyn Fn() + Send + Sync>>,
    persona_overrides: Option<Arc<PersonaOverrides>>,
    /// Agent that serves any workspace without an explicit `agent_bindings`
    /// entry — the scope a runtime engagement override resolves against there,
    /// matching the agent whose `set_mode` writes it.
    default_agent_id: Option<String>,
    /// Slack Web API base for the identity lookups; tests point it at a mock.
    api_base_url: Option<String>,
}

impl SlackSkill {
    pub fn new(config: SlackSkillConfig) -> Self {
        Self {
            config,
            runtimes: Vec::new(),
            secret_store: None,
            monitor_tx: None,
            monitor_on_connect: None,
            persona_overrides: None,
            default_agent_id: None,
            api_base_url: None,
        }
    }

    /// Attach the channel-monitor live tap and reconnect catch-up hook.
    pub fn with_monitor_tap(
        mut self,
        tx: mpsc::Sender<InboundEvent>,
        on_connect: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        self.monitor_tx = Some(tx);
        self.monitor_on_connect = Some(on_connect);
        self
    }

    /// Attach the channel persona override layer (`persona` monitors).
    pub fn with_persona_overrides(mut self, personas: Arc<PersonaOverrides>) -> Self {
        self.persona_overrides = Some(personas);
        self
    }

    /// Set the fallback agent for unbound workspaces (the registry default).
    pub fn with_default_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.default_agent_id = Some(agent_id.into());
        self
    }

    /// Point the identity lookups at another Slack Web API base (a mock).
    pub fn with_api_base_url(mut self, base: impl Into<String>) -> Self {
        self.api_base_url = Some(base.into());
        self
    }

    /// Variant that wires a per-user `SecretStore` for user-scoped Slack
    /// methods (e.g. `search.messages`, which Slack restricts to xoxp-…
    /// user tokens). Pass a `LayeredUserVault` so per-user reads land at
    /// `users/{uid}/skills/slack/{workspace_id}/user_token`.
    pub fn with_store(
        config: SlackSkillConfig,
        secret_store: Arc<dyn workforce_common::settings::SecretStore>,
    ) -> Self {
        Self {
            config,
            runtimes: Vec::new(),
            secret_store: Some(secret_store),
            monitor_tx: None,
            monitor_on_connect: None,
            persona_overrides: None,
            default_agent_id: None,
            api_base_url: None,
        }
    }

    /// Initialize all workspace connections.
    ///
    /// Resolves bot IDs, creates surfaces and event handlers.
    /// Returns a [`SlackStartResult`] with surfaces, action receiver, bindings,
    /// and task handles for clean teardown.
    ///
    /// If `engagement_overrides` is provided, runtime mode changes will be
    /// checked before config-based overrides in the event handler.
    pub async fn start(
        &mut self,
        engagement_overrides: Option<Arc<EngagementOverrides>>,
    ) -> Result<SlackStartResult> {
        let s = self.build_surfaces(engagement_overrides).await?;
        let t = self.start_transport();
        Ok(SlackStartResult {
            surfaces: s.surfaces,
            concrete_surfaces: s.concrete_surfaces,
            workspace_concrete_surfaces: s.workspace_concrete_surfaces,
            workspace_surfaces: s.workspace_surfaces,
            workspace_bot_identities: s.workspace_bot_identities,
            action_rx: t.action_rx,
            action_tx: t.action_tx,
            bindings: s.bindings,
            team_bindings: s.team_bindings,
            task_handles: t.task_handles,
        })
    }

    /// Build outbound surfaces, bot identities, and per-workspace event
    /// handlers WITHOUT opening any socket (only `auth.test` REST calls). Safe
    /// to run on every instance so it can post to Slack; pair with
    /// [`start_transport`](Self::start_transport) on the leader to receive
    /// events. Populates `self.runtimes` for the transport phase.
    pub async fn build_surfaces(
        &mut self,
        engagement_overrides: Option<Arc<EngagementOverrides>>,
    ) -> Result<SlackSurfaces> {
        let mut surfaces: Vec<Arc<dyn ChannelSurface>> = Vec::new();
        let mut workspace_surfaces: std::collections::HashMap<String, Arc<dyn ChannelSurface>> =
            std::collections::HashMap::new();
        let mut workspace_concrete: std::collections::HashMap<String, Arc<SlackSurface>> =
            std::collections::HashMap::new();
        let mut workspace_bot_identities: std::collections::HashMap<String, BotIdentity> =
            std::collections::HashMap::new();
        let mut rejected = std::collections::BTreeMap::new();

        for mut ws_config in self.config.workspaces.clone() {
            let transport = match self.api_base_url.as_deref() {
                Some(base) => transport::SlackTransport::with_base_url(ws_config.clone(), base),
                None => transport::SlackTransport::new(ws_config.clone()),
            };
            let identity = match transport.resolve_bot_identity().await {
                Ok(identity) => identity,
                Err(e) => {
                    warn!(
                        workspace = %ws_config.id,
                        error = %e,
                        "Slack workspace skipped — its bot token was refused"
                    );
                    rejected.insert(ws_config.id.clone(), e.to_string());
                    continue;
                }
            };
            let bot_id = identity.user_id.clone();
            let home_team_id = identity.team_id.clone();
            ws_config.bot_user_id = Some(bot_id.clone());
            info!(
                workspace = %ws_config.id,
                bot_id = %bot_id,
                bot_name = %identity.display_name,
                "Slack bot identity resolved"
            );
            workspace_bot_identities.insert(ws_config.id.clone(), identity);

            let paragraph_spacing = self.config.effective_paragraph_spacing(&ws_config.id);
            let surface = match self.secret_store.as_ref() {
                Some(store) => SlackSurface::shared_with_store(
                    &ws_config.bot_token,
                    Arc::clone(store),
                    ws_config.id.clone(),
                    paragraph_spacing,
                ),
                None => SlackSurface::shared(&ws_config.bot_token, paragraph_spacing),
            };
            match home_team_id {
                Some(team_id) => surface.set_home_team_id(team_id),
                None => warn!(
                    workspace = %ws_config.id,
                    "auth.test returned no team_id — external-sender gating will fail closed"
                ),
            }
            surfaces.push(Arc::clone(&surface) as Arc<dyn ChannelSurface>);
            workspace_surfaces.insert(
                ws_config.id.clone(),
                Arc::clone(&surface) as Arc<dyn ChannelSurface>,
            );
            workspace_concrete.insert(ws_config.id.clone(), Arc::clone(&surface));

            let mut handler = event_handler::SlackEventHandler::new(
                ws_config.id.clone(),
                bot_id,
                self.config.effective_response_mode(&ws_config.id),
                self.config.effective_activation_keywords(&ws_config.id),
                self.config.effective_thread_isolation(&ws_config.id),
                ws_config.channels.clone(),
            )
            .with_channel_overrides(self.config.channel_overrides.clone())
            .with_agent_scope(
                self.config.agent_bindings.clone(),
                self.default_agent_id.clone(),
            );
            if let Some(ref eo) = engagement_overrides {
                handler = handler.with_engagement_overrides(Arc::clone(eo));
            }
            if let Some(ref tx) = self.monitor_tx {
                handler = handler.with_monitor_tx(tx.clone());
            }
            if let Some(ref po) = self.persona_overrides {
                handler = handler.with_persona_overrides(Arc::clone(po));
            }

            self.runtimes.push(WorkspaceRuntime {
                workspace_id: ws_config.id.clone(),
                surface,
                event_handler: Arc::new(handler),
                config: ws_config,
            });
        }

        let concrete_surfaces: Vec<Arc<SlackSurface>> = self
            .runtimes
            .iter()
            .map(|rt| Arc::clone(&rt.surface))
            .collect();

        Ok(SlackSurfaces {
            surfaces,
            concrete_surfaces,
            workspace_concrete_surfaces: workspace_concrete,
            workspace_surfaces,
            workspace_bot_identities,
            bindings: self.config.agent_bindings.clone(),
            team_bindings: self
                .config
                .team_channels
                .iter()
                .map(|tc| (tc.channel_id.clone(), tc.clone()))
                .collect(),
            rejected,
        })
    }

    /// Open the transport sockets and spawn the per-workspace event-handler
    /// tasks for the surfaces built by [`build_surfaces`](Self::build_surfaces).
    /// Leader-only — this is where inbound Slack events are received. Abort the
    /// returned handles to tear the connection down.
    pub fn start_transport(&mut self) -> SlackTransportHandles {
        let (action_tx, action_rx) = mpsc::channel(256);
        let mut task_handles: Vec<JoinHandle<()>> = Vec::new();

        for runtime in &self.runtimes {
            let action_tx = action_tx.clone();
            let mut transport = transport::SlackTransport::new(runtime.config.clone());
            if let Some(ref cb) = self.monitor_on_connect {
                transport = transport.with_on_connect(Arc::clone(cb));
            }
            let (event_tx, mut event_rx) = mpsc::channel(256);

            let transport_handle = tokio::spawn(async move {
                transport.run(event_tx).await;
            });
            task_handles.push(transport_handle);

            let handler = Arc::clone(&runtime.event_handler);
            let handle = tokio::spawn(async move {
                while let Some(payload) = event_rx.recv().await {
                    match handler.handle(payload).await {
                        SlackAction::Ignore => {}
                        action => {
                            if action_tx.send(action).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            task_handles.push(handle);
        }

        info!(workspaces = self.runtimes.len(), "Slack transport started");

        SlackTransportHandles {
            action_rx,
            action_tx,
            task_handles,
        }
    }

    /// Get all tools provided by this skill for a specific workspace.
    ///
    /// If `workspace_id` is None, returns tools for the first workspace.
    pub fn tools(&self, workspace_id: Option<&str>) -> Vec<Arc<dyn Tool>> {
        let runtime = match workspace_id {
            Some(id) => self.runtimes.iter().find(|r| r.workspace_id == id),
            None => self.runtimes.first(),
        };

        match runtime {
            Some(rt) => build_slack_tools(&rt.surface),
            None => vec![],
        }
    }

    /// Slack-specific agent tools that have no platform-agnostic
    /// counterpart in the `messaging.*` family (channel/user enumeration,
    /// search, channel management, file ops). Registered per-agent at
    /// startup by [`crate::adapters::slack::register_per_agent_tools`]
    /// callers; the legacy `slack.send/read/react/channel_info` tools
    /// are intentionally excluded because the `messaging.*` family
    /// already covers them via the generic `ChannelSurface` dispatch.
    pub fn agent_tools(&self, workspace_id: &str) -> Vec<Arc<dyn Tool>> {
        match self
            .runtimes
            .iter()
            .find(|r| r.workspace_id == workspace_id)
        {
            Some(rt) => build_new_slack_tools(&rt.surface),
            None => vec![],
        }
    }

    /// Get surfaces for all workspaces (as trait objects for the generic messaging system).
    pub fn surfaces(&self) -> Vec<Arc<dyn ChannelSurface>> {
        self.runtimes
            .iter()
            .map(|rt| Arc::clone(&rt.surface) as Arc<dyn ChannelSurface>)
            .collect()
    }

    /// Get concrete Slack surfaces for file downloads and other Slack-specific operations.
    pub fn slack_surfaces(&self) -> Vec<Arc<SlackSurface>> {
        self.runtimes
            .iter()
            .map(|rt| Arc::clone(&rt.surface))
            .collect()
    }

    /// Get the skill definition for the registry.
    pub fn definition() -> SkillDefinition {
        SkillDefinition {
            name: "slack-integration".to_string(),
            version: "1.0.0".to_string(),
            description: "Send messages, react, and receive events from Slack channels".to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),
            tools: vec![
                SkillTool {
                    name: "slack.send".to_string(),
                    description: "Send a message to a Slack channel or thread".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "channel": { "type": "string" },
                            "text": { "type": "string" },
                            "thread_ts": { "type": "string" }
                        },
                        "required": ["channel", "text"]
                    }),
                },
                SkillTool {
                    name: "slack.react".to_string(),
                    description: "Add an emoji reaction to a Slack message".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "channel": { "type": "string" },
                            "timestamp": { "type": "string" },
                            "emoji": { "type": "string" }
                        },
                        "required": ["channel", "timestamp", "emoji"]
                    }),
                },
                SkillTool {
                    name: "slack.read".to_string(),
                    description: "Read recent messages from a Slack channel".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "channel": { "type": "string" },
                            "limit": { "type": "integer", "default": 20, "description": "Number of messages, integer (default: 20)" }
                        },
                        "required": ["channel"]
                    }),
                },
                SkillTool {
                    name: "slack.channel_info".to_string(),
                    description: "Look up Slack channel info including human-readable name"
                        .to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "channel": { "type": "string" }
                        },
                        "required": ["channel"]
                    }),
                },
            ]
            .into_iter()
            .chain(new_slack_skill_tools())
            .collect(),
            event_sources: vec![SkillEventSource {
                r#type: "slack_socket_mode".to_string(),
                events: vec![
                    "slack.message".to_string(),
                    "slack.mention".to_string(),
                    "slack.reaction".to_string(),
                ],
                config_required: vec!["bot_token".to_string(), "app_token".to_string()],
            }],
            default_policies: NEW_SLACK_TOOL_NAMES
                .iter()
                .chain(LEGACY_SLACK_TOOL_NAMES.iter())
                .map(|name| SkillPolicy {
                    tool: (*name).to_string(),
                    action: "allow".to_string(),
                })
                .collect(),
            memory_schemas: Vec::new(),
            requirements: Default::default(),
            dependencies: Vec::new(),
        }
    }
}

/// Pre-existing Slack tools shipped before DW-agent-tools. Kept as a
/// separate constant so the new-tool list is the only thing to extend
/// when more land.
const LEGACY_SLACK_TOOL_NAMES: &[&str] = &[
    "slack.read",
    "slack.send",
    "slack.react",
    "slack.channel_info",
];

/// Tools added for the agent-grade Slack surface. Order mirrors the
/// `tools()` factory and the `definition()` catalogue so a single test
/// can assert all three lists stay in lockstep.
pub const NEW_SLACK_TOOL_NAMES: &[&str] = &[
    "slack.list_channels",
    "slack.list_users",
    "slack.lookup_user_by_email",
    "slack.list_channel_members",
    "slack.list_user_conversations",
    "slack.search_messages",
    "slack.get_message_replies",
    "slack.user_info",
    "slack.invite_to_channel",
    "slack.update_message",
    "slack.delete_message",
    "slack.pin_message",
    "slack.unpin_message",
    "slack.create_channel",
    "slack.archive_channel",
    "slack.unarchive_channel",
    "slack.set_channel_metadata",
    "slack.rename_channel",
    "slack.kick_from_channel",
    "slack.upload_file",
    "slack.list_files",
    "slack.delete_file",
];

/// Read-only Slack agent tools — the worker-acquirable subset of
/// [`NEW_SLACK_TOOL_NAMES`] (the tools `SlackManager` actually registers per
/// agent). Explicit names, never a `slack.*` glob: the agent surface mixes
/// reads and writes under one prefix (`slack.list_channels` vs
/// `slack.delete_message`), so a prefix match would hand a read-only worker
/// write tools. (`slack.read`/`slack.channel_info` are legacy `messaging.*`
/// tools, not part of this agent surface.)
pub const AGENT_READ_TOOLS: &[&str] = &[
    "slack.list_channels",
    "slack.list_users",
    "slack.lookup_user_by_email",
    "slack.list_channel_members",
    "slack.list_user_conversations",
    "slack.search_messages",
    "slack.get_message_replies",
    "slack.user_info",
    "slack.list_files",
];

/// Write/mutating Slack agent tools — never offered to a read-only worker.
pub const AGENT_WRITE_TOOLS: &[&str] = &[
    "slack.invite_to_channel",
    "slack.update_message",
    "slack.delete_message",
    "slack.pin_message",
    "slack.unpin_message",
    "slack.create_channel",
    "slack.archive_channel",
    "slack.unarchive_channel",
    "slack.set_channel_metadata",
    "slack.rename_channel",
    "slack.kick_from_channel",
    "slack.upload_file",
    "slack.delete_file",
];

/// One factory that knows how to wire every Slack tool to its workspace
/// surface. `SlackSkill::tools()` uses this against the live runtime
/// surface; `new_slack_skill_tools()` uses it against a dummy surface
/// purely to extract `SkillDefinition` metadata.
fn build_slack_tools(surface: &Arc<SlackSurface>) -> Vec<Arc<dyn Tool>> {
    let channel_surface: Arc<dyn ChannelSurface> = Arc::clone(surface) as Arc<dyn ChannelSurface>;
    let mut new_tools: Vec<Arc<dyn Tool>> = build_new_slack_tools(surface);
    let mut out: Vec<Arc<dyn Tool>> = vec![
        Arc::new(tools::SlackSendTool::new(Arc::clone(&channel_surface))),
        Arc::new(tools::SlackReactTool::new(Arc::clone(&channel_surface))),
        Arc::new(tools::SlackReadTool::new(channel_surface)),
        Arc::new(tools::SlackChannelInfoTool::new(Arc::clone(surface))),
    ];
    out.append(&mut new_tools);
    out
}

/// Build the agent-grade Slack tools (the 22 added for DW). Separate
/// from `build_slack_tools` because the legacy tools take an
/// `Arc<dyn ChannelSurface>` while these take the concrete
/// `Arc<SlackSurface>`.
fn build_new_slack_tools(surface: &Arc<SlackSurface>) -> Vec<Arc<dyn Tool>> {
    let s = || Arc::clone(surface);
    vec![
        Arc::new(tools::SlackListChannelsTool::new(s())),
        Arc::new(tools::SlackListUsersTool::new(s())),
        Arc::new(tools::SlackLookupUserByEmailTool::new(s())),
        Arc::new(tools::SlackListChannelMembersTool::new(s())),
        Arc::new(tools::SlackListUserConversationsTool::new(s())),
        Arc::new(tools::SlackSearchMessagesTool::new(s())),
        Arc::new(tools::SlackGetMessageRepliesTool::new(s())),
        Arc::new(tools::SlackUserInfoTool::new(s())),
        Arc::new(tools::SlackInviteToChannelTool::new(s())),
        Arc::new(tools::SlackUpdateMessageTool::new(s())),
        Arc::new(tools::SlackDeleteMessageTool::new(s())),
        Arc::new(tools::SlackPinMessageTool::new(s())),
        Arc::new(tools::SlackUnpinMessageTool::new(s())),
        Arc::new(tools::SlackCreateChannelTool::new(s())),
        Arc::new(tools::SlackArchiveChannelTool::new(s())),
        Arc::new(tools::SlackUnarchiveChannelTool::new(s())),
        Arc::new(tools::SlackSetChannelMetadataTool::new(s())),
        Arc::new(tools::SlackRenameChannelTool::new(s())),
        Arc::new(tools::SlackKickFromChannelTool::new(s())),
        Arc::new(tools::SlackUploadFileTool::new(s())),
        Arc::new(tools::SlackListFilesTool::new(s())),
        Arc::new(tools::SlackDeleteFileTool::new(s())),
    ]
}

/// `SkillDefinition` catalogue entries for the 22 new tools, extracted
/// by introspecting each `Tool` impl. The dummy surface here is only
/// used for name/description/schema extraction; nothing goes over the
/// wire from `definition()`.
fn new_slack_skill_tools() -> Vec<SkillTool> {
    let surface = Arc::new(SlackSurface::new(concat!("xoxb-d", "efinition-only")));
    build_new_slack_tools(&surface)
        .into_iter()
        .map(|t| SkillTool {
            name: t.name().to_string(),
            description: t.description().to_string(),
            input_schema: t.input_schema(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tool_names_match_built_agent_tools() {
        let surface = Arc::new(SlackSurface::new("xoxb-test"));
        let built: Vec<String> = build_new_slack_tools(&surface)
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        let declared: Vec<String> = NEW_SLACK_TOOL_NAMES.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            built, declared,
            "NEW_SLACK_TOOL_NAMES drifted from build_new_slack_tools()"
        );
    }

    #[tokio::test]
    async fn a_refused_workspace_is_left_out_without_failing_the_others() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth.test"))
            .and(header("authorization", "Bearer xoxb-good"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "user_id": "U_GOOD", "team_id": "T1",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth.test"))
            .and(header("authorization", "Bearer xoxb-revoked"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": false, "error": "token_revoked",
            })))
            .mount(&server)
            .await;
        let config: SlackSkillConfig = serde_json::from_value(serde_json::json!({
            "workspaces": [
                { "id": "good", "bot_token": "xoxb-good", "app_token": "xapp-good" },
                { "id": "gone", "bot_token": "xoxb-revoked", "app_token": "xapp-gone" },
            ],
        }))
        .unwrap();

        let built = SlackSkill::new(config)
            .with_api_base_url(server.uri())
            .build_surfaces(None)
            .await
            .unwrap();

        assert_eq!(
            built.workspace_surfaces.keys().collect::<Vec<_>>(),
            vec!["good"]
        );
        assert_eq!(built.workspace_bot_identities["good"].user_id, "U_GOOD");
        assert!(built.rejected["gone"].contains("token_revoked"));
    }

    #[test]
    fn agent_read_write_lists_cover_all_agent_tools() {
        crate::adapters::test_support::assert_read_write_partition(
            NEW_SLACK_TOOL_NAMES.to_vec(),
            AGENT_READ_TOOLS,
            AGENT_WRITE_TOOLS,
        );
    }
}

//! Slack app tools: manifest scaffolding, per-user tokens, and connecting an
//! agent to Slack (the last through [`SlackControl`]).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use tracing::{info, warn};
use workforce_agent::tools::{Tool, ToolResult};

use workforce_common::settings::{SecretStore, VaultActor};

use crate::slack_onboarding::{ConnectRequest, SlackControl};
use crate::vault::Vault;

/// Shared context for Slack tools.
///
/// `vault` holds the agent-shared vault used for workspace-scoped bot/app/
/// signing keys. `secret_store` is a `LayeredUserVault` wrapping the same
/// agent vault plus the global vault, used for per-user xoxp-… token
/// reads/writes (which auto-prefix `users/{uid}/…`).
#[derive(Clone)]
pub struct SlackToolContext {
    pub vault: Arc<Vault>,
    pub secret_store: Arc<dyn SecretStore>,
    pub data_dir: String,
}

/// Create all Slack scaffolding tools.
pub fn slack_tools(ctx: SlackToolContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(SlackAppScaffoldTool { ctx: ctx.clone() }),
        Arc::new(SlackUserTokenSetTool { ctx: ctx.clone() }),
        Arc::new(SlackUserTokenClearTool { ctx }),
    ]
}

/// The tools that connect `agent_id` to Slack, reload it, and report how it
/// stands. `slack.app_configure` is the earlier name of `slack.connect`.
pub fn onboarding_tools(control: Arc<SlackControl>, agent_id: &str) -> Vec<Arc<dyn Tool>> {
    let connect = |name| -> Arc<dyn Tool> {
        Arc::new(SlackConnectTool {
            control: Arc::clone(&control),
            agent_id: agent_id.to_string(),
            name,
        })
    };
    vec![
        connect("slack.connect"),
        connect("slack.app_configure"),
        Arc::new(SlackDisconnectTool {
            control: Arc::clone(&control),
            agent_id: agent_id.to_string(),
        }),
        Arc::new(SlackStatusTool {
            control: Arc::clone(&control),
            agent_id: agent_id.to_string(),
        }),
        Arc::new(SystemReloadSlackTool::new(control)),
    ]
}

/// Who a write made by `agent_id`'s turn is recorded as: the person the turn
/// runs for, else the agent.
fn turn_actor<'a>(user: Option<&'a str>, agent_id: &'a str) -> VaultActor<'a> {
    user.map_or(VaultActor::Agent(agent_id), VaultActor::User)
}

/// Generate a valid Slack app manifest YAML for workforce integration.
///
/// Scopes cover public channels, private channels, 1:1 DMs, and group DMs.
/// Event subscriptions match the scopes so the bot wakes up on relevant
/// activity in any conversation type it is invited to. `app_mention` is
/// included so explicit `@mention` events arrive on their own subscription
/// (more reliable than relying on `message.channels` keyword scan), and
/// `member_joined_channel` / `member_left_channel` let the routing layer
/// invalidate caches when the bot is added or removed from a channel.
pub fn generate_manifest(app_name: &str, bot_name: &str) -> String {
    format!(
        r#"display_information:
  name: "{app_name}"
  description: "Workforce AI agent integration"

features:
  bot_user:
    display_name: "{bot_name}"
    always_online: true
  app_home:
    home_tab_enabled: false
    messages_tab_enabled: true
    messages_tab_read_only_enabled: false

oauth_config:
  scopes:
    bot:
      - app_mentions:read
      - channels:history
      - channels:read
      - chat:write
      - chat:write.public
      - files:read
      - files:write
      - groups:history
      - groups:read
      - groups:write.invites
      - im:history
      - im:read
      - im:write
      - mpim:history
      - mpim:read
      - mpim:write
      - reactions:read
      - reactions:write
      - users:read
      - users:read.email

settings:
  event_subscriptions:
    bot_events:
      - app_mention
      - member_joined_channel
      - member_left_channel
      - message.channels
      - message.groups
      - message.im
      - message.mpim
  interactivity:
    is_enabled: true
  org_deploy_enabled: false
  socket_mode_enabled: true
  token_rotation_enabled: false
"#
    )
}

// ─── slack.app_scaffold ──────────────────────────────────────────────────────

struct SlackAppScaffoldTool {
    #[allow(dead_code)]
    ctx: SlackToolContext,
}

#[derive(Deserialize)]
struct ScaffoldInput {
    app_name: String,
    #[serde(default)]
    bot_name: Option<String>,
}

#[async_trait]
impl Tool for SlackAppScaffoldTool {
    fn name(&self) -> &str {
        "slack.app_scaffold"
    }

    fn description(&self) -> &str {
        "Generate a Slack app manifest for workforce integration"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "app_name": {
                    "type": "string",
                    "description": "The name for the Slack app"
                },
                "bot_name": {
                    "type": "string",
                    "description": "Bot display name (defaults to app_name if not provided)"
                }
            },
            "required": ["app_name"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: ScaffoldInput = serde_json::from_value(input)?;

        let bot = params.bot_name.as_deref().unwrap_or(&params.app_name);
        let manifest = generate_manifest(&params.app_name, bot);

        let instructions = vec![
            "Go to https://api.slack.com/apps",
            "Click 'Create New App' → 'From an app manifest'",
            "Select your workspace",
            "Paste the manifest above (switch to YAML mode)",
            "Install the app to your workspace",
            "Under Basic Information create an app-level token with the connections:write scope (xapp-…)",
            "Call slack.connect with the Bot User OAuth Token (xoxb-…) and the app-level token",
        ];

        Ok(ToolResult::success(
            serde_json::json!({
                "manifest": manifest,
                "app_name": params.app_name,
                "bot_name": bot,
                "instructions": instructions,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── slack.connect ───────────────────────────────────────────────────────────

struct SlackConnectTool {
    control: Arc<SlackControl>,
    agent_id: String,
    name: &'static str,
}

#[async_trait]
impl Tool for SlackConnectTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "Connect this agent to Slack with the tokens of a Slack app made for it \
         (slack.app_scaffold generates the manifest). Verifies both tokens with Slack, \
         refuses an app another agent already uses, stores the tokens in this agent's \
         vault, binds the workspace to this agent and reconnects Slack on every hub \
         replica. Call it again with new tokens to rotate them."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "bot_token": {
                    "type": "string",
                    "description": "Bot User OAuth Token (xoxb-...) from the app's OAuth & Permissions page"
                },
                "app_token": {
                    "type": "string",
                    "description": "App-level token for Socket Mode (xapp-...) with the connections:write scope"
                },
                "signing_secret": {
                    "type": "string",
                    "description": "Signing secret from the app's Basic Information page (optional)"
                },
                "workspace_id": {
                    "type": "string",
                    "description": "Workspace connection id — the `id` of this agent's [[workspaces]] entry in the Slack skill config. Usually omitted: the agent's existing workspace is reused, or a new one named after the agent is created."
                }
            },
            "required": ["bot_token", "app_token"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let Ok(request) = serde_json::from_value::<ConnectRequest>(input) else {
            return Ok(ToolResult::failure(
                format!(
                    "{} needs bot_token (xoxb-…) and app_token (xapp-…) as strings",
                    self.name
                ),
                start.elapsed().as_millis() as u64,
            ));
        };
        let user = current_user_id();
        let outcome = self
            .control
            .connect(
                &self.agent_id,
                request,
                turn_actor(user.as_deref(), &self.agent_id),
            )
            .await;
        let elapsed = start.elapsed().as_millis() as u64;
        Ok(match outcome {
            Ok(connection) => ToolResult::success(serde_json::to_value(connection)?, elapsed),
            Err(refusal) => ToolResult::failure(refusal.to_string(), elapsed),
        })
    }
}

// ─── slack.disconnect ────────────────────────────────────────────────────────

struct SlackDisconnectTool {
    control: Arc<SlackControl>,
    agent_id: String,
}

#[derive(Deserialize, Default)]
struct DisconnectInput {
    #[serde(default)]
    workspace_id: Option<String>,
}

#[async_trait]
impl Tool for SlackDisconnectTool {
    fn name(&self) -> &str {
        "slack.disconnect"
    }

    fn description(&self) -> &str {
        "Disconnect this agent from Slack: removes its workspace's tokens from the vault, \
         its entry and binding from the Slack skill config, and its connection on every \
         hub replica."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace_id": {
                    "type": "string",
                    "description": "Only this workspace, when the agent has several (default: all of them)"
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: DisconnectInput = if input.is_null() {
            DisconnectInput::default()
        } else {
            serde_json::from_value(input)?
        };
        let user = current_user_id();
        let outcome = self
            .control
            .disconnect(
                &self.agent_id,
                params.workspace_id.as_deref(),
                turn_actor(user.as_deref(), &self.agent_id),
            )
            .await;
        let elapsed = start.elapsed().as_millis() as u64;
        Ok(match outcome {
            Ok(done) => ToolResult::success(serde_json::to_value(done)?, elapsed),
            Err(refusal) => ToolResult::failure(refusal.to_string(), elapsed),
        })
    }
}

// ─── slack.status ────────────────────────────────────────────────────────────

struct SlackStatusTool {
    control: Arc<SlackControl>,
    agent_id: String,
}

#[async_trait]
impl Tool for SlackStatusTool {
    fn name(&self) -> &str {
        "slack.status"
    }

    fn description(&self) -> &str {
        "Show this agent's Slack workspaces: whether each is connected, as which bot, \
         and why not when it isn't."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let outcome = self.control.status(&self.agent_id).await;
        let elapsed = start.elapsed().as_millis() as u64;
        Ok(match outcome {
            Ok(status) => ToolResult::success(serde_json::to_value(status)?, elapsed),
            Err(refusal) => ToolResult::failure(refusal.to_string(), elapsed),
        })
    }
}

// ─── slack.user_token_set ────────────────────────────────────────────────────

struct SlackUserTokenSetTool {
    ctx: SlackToolContext,
}

#[derive(Deserialize)]
struct UserTokenSetInput {
    workspace_id: String,
    user_token: String,
}

fn current_user_id() -> Option<String> {
    workforce_agent::runner::CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty())
}

fn user_token_path(workspace_id: &str) -> String {
    format!("skills/slack/{workspace_id}/user_token")
}

#[async_trait]
impl Tool for SlackUserTokenSetTool {
    fn name(&self) -> &str {
        "slack.user_token_set"
    }

    fn description(&self) -> &str {
        "Register the calling user's personal Slack xoxp-… token for a workspace. \
         Required for slack.search_messages (Slack's search endpoint refuses bot tokens). \
         Stored per-user; never shared across humans."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace_id": {
                    "type": "string",
                    "description": "Workspace connection ID matching the 'id' field in the slack skill config"
                },
                "user_token": {
                    "type": "string",
                    "description": "User OAuth token (xoxp-...) with the required user scopes (e.g. search:read)"
                }
            },
            "required": ["workspace_id", "user_token"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: UserTokenSetInput = serde_json::from_value(input)?;
        let elapsed = || start.elapsed().as_millis() as u64;

        let Some(uid) = current_user_id() else {
            return Ok(ToolResult::failure(
                "slack.user_token_set requires a user identity in the request context — \
                 this call cannot run in a system/background context.",
                elapsed(),
            ));
        };
        if params.user_token.is_empty() {
            return Ok(ToolResult::failure(
                "slack.user_token_set: user_token must be a non-empty xoxp-… string.",
                elapsed(),
            ));
        }

        let path = user_token_path(&params.workspace_id);
        self.ctx
            .secret_store
            .set_with_actor(&path, &params.user_token, VaultActor::User(&uid))?;

        Ok(ToolResult::success(
            serde_json::json!({
                "stored_at_bare_path": path,
                "workspace_id": params.workspace_id,
                "user_id": uid,
                "note": "Token stored per-user. Other users in the same workspace must register their own.",
            }),
            elapsed(),
        ))
    }
}

// ─── slack.user_token_clear ──────────────────────────────────────────────────

struct SlackUserTokenClearTool {
    ctx: SlackToolContext,
}

#[derive(Deserialize)]
struct UserTokenClearInput {
    workspace_id: String,
}

#[async_trait]
impl Tool for SlackUserTokenClearTool {
    fn name(&self) -> &str {
        "slack.user_token_clear"
    }

    fn description(&self) -> &str {
        "Remove the calling user's personal Slack xoxp-… token for a workspace. \
         Safe to call when nothing is registered (returns existed=false)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace_id": {
                    "type": "string",
                    "description": "Workspace connection ID matching the 'id' field in the slack skill config"
                }
            },
            "required": ["workspace_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: UserTokenClearInput = serde_json::from_value(input)?;
        let elapsed = || start.elapsed().as_millis() as u64;

        let Some(uid) = current_user_id() else {
            return Ok(ToolResult::failure(
                "slack.user_token_clear requires a user identity in the request context.",
                elapsed(),
            ));
        };

        let path = user_token_path(&params.workspace_id);
        let existed = self.ctx.secret_store.get_user_override(&path).is_some();
        if existed {
            self.ctx.secret_store.delete(&path)?;
        }

        Ok(ToolResult::success(
            serde_json::json!({
                "bare_path": path,
                "workspace_id": params.workspace_id,
                "user_id": uid,
                "existed": existed,
            }),
            elapsed(),
        ))
    }
}

// ─── system.reload_slack ─────────────────────────────────────────────────────

pub struct SystemReloadSlackTool {
    slack: Arc<SlackControl>,
}

impl SystemReloadSlackTool {
    pub fn new(slack: Arc<SlackControl>) -> Self {
        Self { slack }
    }
}

#[derive(Deserialize, Default)]
struct ReloadSlackInput {
    /// Workspace connection ID — only required when overriding tokens for
    /// a specific workspace. Omit (along with the token fields) to reload
    /// every workspace from disk + vault with no overrides.
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    bot_token: Option<String>,
    #[serde(default)]
    app_token: Option<String>,
}

#[async_trait]
impl Tool for SystemReloadSlackTool {
    fn name(&self) -> &str {
        "system.reload_slack"
    }

    fn description(&self) -> &str {
        "Reload the Slack integration on every hub replica. Call with no arguments to \
         reload all workspaces from disk + vault, or pass workspace_id + \
         bot_token/app_token to also persist new tokens for one workspace. To connect \
         this agent to Slack use slack.connect instead."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace_id": {
                    "type": "string",
                    "description": "Workspace connection ID (only required when overriding tokens)"
                },
                "bot_token": {
                    "type": "string",
                    "description": "Optional new bot token (xoxb-...) — requires workspace_id"
                },
                "app_token": {
                    "type": "string",
                    "description": "Optional new app token (xapp-...) — requires workspace_id"
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ReloadSlackInput = if input.is_null() {
            ReloadSlackInput::default()
        } else {
            serde_json::from_value(input)?
        };
        let duration = || start.elapsed().as_millis() as u64;

        let has_token_override = params.bot_token.is_some() || params.app_token.is_some();
        let overrides = match (params.workspace_id, has_token_override) {
            (Some(workspace_id), true) => Some(crate::slack_manager::SlackTokenOverrides {
                workspace_id,
                bot_token: params.bot_token,
                app_token: params.app_token,
            }),
            (None, true) => {
                return Ok(ToolResult::failure(
                    "bot_token/app_token requires workspace_id".to_string(),
                    duration(),
                ));
            }
            _ => None,
        };

        match self.slack.reload(overrides).await {
            Ok(result) => {
                info!(
                    was_running = result.was_running,
                    workspaces = result.workspaces,
                    "Slack reloaded via system.reload_slack"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "reloaded": true,
                        "was_running": result.was_running,
                        "workspaces": result.workspaces,
                        "unconnected": result.unconnected,
                    }),
                    duration(),
                ))
            }
            Err(e) => {
                warn!(error = %e, "system.reload_slack failed");
                Ok(ToolResult::failure(
                    format!("Failed to reload Slack: {}", e),
                    duration(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::LayeredUserVault;
    use workforce_agent::runner::CURRENT_USER_ID;

    fn test_key() -> String {
        "test-slack-tool-key".to_string()
    }

    fn fresh_ctx() -> (tempfile::TempDir, Arc<Vault>, SlackToolContext) {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");
        let vault = Arc::new(Vault::open(&vault_path, &test_key()).unwrap());
        let secret_store: Arc<dyn SecretStore> =
            Arc::new(LayeredUserVault::new(Arc::clone(&vault), None));
        let ctx = SlackToolContext {
            vault: Arc::clone(&vault),
            secret_store,
            data_dir: tmp.path().to_string_lossy().to_string(),
        };
        (tmp, vault, ctx)
    }

    #[test]
    fn the_manifest_enables_interactivity() {
        let manifest: serde_yaml::Value =
            serde_yaml::from_str(&generate_manifest("Workforce", "workforce")).unwrap();
        assert_eq!(
            manifest["settings"]["interactivity"]["is_enabled"],
            serde_yaml::Value::Bool(true)
        );
    }

    #[test]
    fn manifest_contains_app_name() {
        let manifest = generate_manifest("my-workforce-bot", "my-workforce-bot");
        assert!(manifest.contains("my-workforce-bot"));
        assert!(manifest.contains(r#"name: "my-workforce-bot""#));
    }

    #[test]
    fn manifest_uses_bot_name_when_provided() {
        let manifest = generate_manifest("my-app", "custom-bot");
        assert!(manifest.contains(r#"name: "my-app""#));
        assert!(manifest.contains(r#"display_name: "custom-bot""#));
    }

    #[test]
    fn manifest_uses_app_name_as_default_bot_name() {
        let manifest = generate_manifest("workforce-bot", "workforce-bot");
        assert!(manifest.contains(r#"name: "workforce-bot""#));
        assert!(manifest.contains(r#"display_name: "workforce-bot""#));
    }

    #[test]
    fn manifest_includes_every_required_scope_and_event() {
        let manifest = generate_manifest("wf", "wf");
        let required_scopes = [
            "app_mentions:read",
            "channels:history",
            "channels:read",
            "chat:write",
            "chat:write.public",
            "files:read",
            "files:write",
            "groups:history",
            "groups:read",
            "groups:write.invites",
            "im:history",
            "im:read",
            "im:write",
            "mpim:history",
            "mpim:read",
            "mpim:write",
            "reactions:read",
            "reactions:write",
            "users:read",
            "users:read.email",
        ];
        for scope in required_scopes {
            assert!(
                manifest.contains(scope),
                "manifest missing required scope: {scope}"
            );
        }
        let required_events = [
            "app_mention",
            "member_joined_channel",
            "member_left_channel",
            "message.channels",
            "message.groups",
            "message.im",
            "message.mpim",
        ];
        for event in required_events {
            assert!(
                manifest.contains(event),
                "manifest missing required event subscription: {event}"
            );
        }
    }

    #[test]
    fn manifest_yaml_parses_and_lists_match_expected() {
        let manifest = generate_manifest("wf", "wf");
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&manifest).expect("manifest must be valid YAML");
        let scopes = parsed["oauth_config"]["scopes"]["bot"]
            .as_sequence()
            .expect("bot scopes")
            .iter()
            .map(|s| s.as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>();
        assert!(
            scopes.windows(2).all(|w| w[0] <= w[1]),
            "scopes must be alphabetised: {scopes:?}"
        );
        let events = parsed["settings"]["event_subscriptions"]["bot_events"]
            .as_sequence()
            .expect("bot events")
            .iter()
            .map(|s| s.as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>();
        assert!(
            events.windows(2).all(|w| w[0] <= w[1]),
            "events must be alphabetised: {events:?}"
        );
    }

    #[tokio::test]
    async fn user_token_set_stores_at_per_user_bare_path() {
        let (_tmp, vault, ctx) = fresh_ctx();
        let tool = SlackUserTokenSetTool { ctx };
        let fut = async {
            tool.execute(serde_json::json!({
                "workspace_id": "allan",
                "user_token": "xoxp-kingsley"
            }))
            .await
        };
        let result = CURRENT_USER_ID
            .scope("kingsley".to_string(), fut)
            .await
            .unwrap();
        assert!(result.success);

        assert_eq!(
            vault
                .get("users/kingsley/skills/slack/allan/user_token")
                .unwrap(),
            "xoxp-kingsley"
        );
        assert!(vault.get("skills/slack/allan/user_token").is_none());
        assert!(vault.get("slack.allan.user_token").is_none());

        let entry = vault
            .get_entry("users/kingsley/skills/slack/allan/user_token")
            .expect("audit entry");
        assert_eq!(entry.updated_by, "user:kingsley");
    }

    #[tokio::test]
    async fn user_token_set_refuses_without_user_identity() {
        let (_tmp, vault, ctx) = fresh_ctx();
        let tool = SlackUserTokenSetTool { ctx };
        let result = tool
            .execute(serde_json::json!({
                "workspace_id": "allan",
                "user_token": "xoxp-x"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert_eq!(vault.list(Some("users/")).len(), 0);
        assert_eq!(vault.list(Some("skills/")).len(), 0);
    }

    #[tokio::test]
    async fn user_token_set_isolates_per_user_writes() {
        let (_tmp, vault, ctx) = fresh_ctx();
        let tool = SlackUserTokenSetTool { ctx };
        CURRENT_USER_ID
            .scope("kingsley".to_string(), async {
                tool.execute(serde_json::json!({
                    "workspace_id": "allan",
                    "user_token": "xoxp-kings"
                }))
                .await
                .unwrap();
            })
            .await;
        CURRENT_USER_ID
            .scope("brian".to_string(), async {
                tool.execute(serde_json::json!({
                    "workspace_id": "allan",
                    "user_token": "xoxp-brian"
                }))
                .await
                .unwrap();
            })
            .await;
        assert_eq!(
            vault
                .get("users/kingsley/skills/slack/allan/user_token")
                .unwrap(),
            "xoxp-kings"
        );
        assert_eq!(
            vault
                .get("users/brian/skills/slack/allan/user_token")
                .unwrap(),
            "xoxp-brian"
        );
    }

    #[tokio::test]
    async fn user_token_set_refuses_empty_string() {
        let (_tmp, vault, ctx) = fresh_ctx();
        let tool = SlackUserTokenSetTool { ctx };
        let result = CURRENT_USER_ID
            .scope(
                "kingsley".to_string(),
                tool.execute(serde_json::json!({
                    "workspace_id": "allan",
                    "user_token": ""
                })),
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(vault.list(Some("users/")).is_empty());
    }

    #[tokio::test]
    async fn user_token_clear_removes_per_user_key() {
        let (_tmp, vault, ctx) = fresh_ctx();
        let set_tool = SlackUserTokenSetTool { ctx: ctx.clone() };
        let clear_tool = SlackUserTokenClearTool { ctx };
        CURRENT_USER_ID
            .scope("kingsley".to_string(), async {
                set_tool
                    .execute(serde_json::json!({
                        "workspace_id": "allan",
                        "user_token": "xoxp-kings"
                    }))
                    .await
                    .unwrap();
                let res = clear_tool
                    .execute(serde_json::json!({"workspace_id": "allan"}))
                    .await
                    .unwrap();
                assert!(res.success);
                assert_eq!(res.output["existed"], true);
            })
            .await;
        assert!(vault
            .get("users/kingsley/skills/slack/allan/user_token")
            .is_none());
    }

    #[tokio::test]
    async fn user_token_clear_returns_existed_false_when_nothing_to_clear() {
        let (_tmp, _vault, ctx) = fresh_ctx();
        let tool = SlackUserTokenClearTool { ctx };
        let res = CURRENT_USER_ID
            .scope(
                "kingsley".to_string(),
                tool.execute(serde_json::json!({"workspace_id": "allan"})),
            )
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["existed"], false);
    }

    #[test]
    fn reload_slack_input_accepts_empty_object() {
        let parsed: ReloadSlackInput = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(parsed.workspace_id.is_none());
        assert!(parsed.bot_token.is_none());
        assert!(parsed.app_token.is_none());
    }

    #[test]
    fn reload_slack_input_accepts_full_payload() {
        let parsed: ReloadSlackInput = serde_json::from_value(serde_json::json!({
            "workspace_id": "default",
            "bot_token": "xoxb-test",
            "app_token": "xapp-test",
        }))
        .unwrap();
        assert_eq!(parsed.workspace_id.as_deref(), Some("default"));
        assert_eq!(parsed.bot_token.as_deref(), Some("xoxb-test"));
        assert_eq!(parsed.app_token.as_deref(), Some("xapp-test"));
    }

    #[test]
    fn reload_slack_input_default_matches_empty_object() {
        let from_default = ReloadSlackInput::default();
        let from_empty: ReloadSlackInput = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(from_default.workspace_id, from_empty.workspace_id);
        assert_eq!(from_default.bot_token, from_empty.bot_token);
        assert_eq!(from_default.app_token, from_empty.app_token);
    }
}

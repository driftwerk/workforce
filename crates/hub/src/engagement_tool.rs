//! Engagement mode tools — runtime configuration of Slack response modes.
//!
//! Tools: `messaging.set_mode`, `messaging.get_mode`, `messaging.list_modes`.
//! Each tool is bound to the agent it is registered on, so an override applies
//! only to that bot, never to others sharing the same channel.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::engagement::{EngagementOverrides, EngagementScope};
use workforce_common::settings::ResponseMode;

use crate::serve::AgentService;
use crate::vault::Vault;

fn mode_str(mode: &ResponseMode) -> &'static str {
    match mode {
        ResponseMode::All => "all",
        ResponseMode::MentionOnly => "mention_only",
        ResponseMode::Quiet => "quiet",
    }
}

/// Persist the override store to the vault — shared by the agent tools and the
/// admin API so both survive a restart.
pub async fn persist_overrides(vault: Option<&Arc<Vault>>, overrides: &EngagementOverrides) {
    if let Some(vault) = vault {
        let json = overrides.snapshot_json().await;
        if let Err(e) = vault.set("engagement/overrides", &json) {
            tracing::warn!("Failed to persist engagement overrides to vault: {}", e);
        }
    }
}

/// Register the engagement-mode tool family on one agent against the shared
/// hub store. Called from both the boot wiring loop and the live-spawn path so
/// every agent — including config-overlay spawns — gets a scoped `set_mode`.
pub fn register_engagement_tools(
    service: &Arc<AgentService>,
    overrides: &Arc<EngagementOverrides>,
    vault: Option<Arc<Vault>>,
) {
    let agent_id = service.agent_id.as_str();
    service.register_tool(Arc::new(MessagingSetModeTool::new(
        agent_id,
        Arc::clone(overrides),
        vault,
    )));
    service.register_tool(Arc::new(MessagingGetModeTool::new(
        agent_id,
        Arc::clone(overrides),
    )));
    service.register_tool(Arc::new(MessagingListModesTool::new(
        agent_id,
        Arc::clone(overrides),
    )));
}

// ---------------------------------------------------------------------------
// messaging.set_mode
// ---------------------------------------------------------------------------

pub struct MessagingSetModeTool {
    agent_id: String,
    overrides: Arc<EngagementOverrides>,
    vault: Option<Arc<Vault>>,
}

impl MessagingSetModeTool {
    pub fn new(
        agent_id: impl Into<String>,
        overrides: Arc<EngagementOverrides>,
        vault: Option<Arc<Vault>>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            overrides,
            vault,
        }
    }

    async fn persist_to_vault(&self) {
        persist_overrides(self.vault.as_ref(), &self.overrides).await;
    }
}

#[derive(Deserialize)]
struct SetModeInput {
    channel: String,
    #[serde(default)]
    thread_id: Option<String>,
    mode: String,
}

#[async_trait]
impl Tool for MessagingSetModeTool {
    fn name(&self) -> &str {
        "messaging.set_mode"
    }

    fn description(&self) -> &str {
        "Set this agent's engagement mode for a channel/thread (all, mention_only, quiet, default). \
         Applies only to you, not to other bots in the channel."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Channel ID to set the mode for"
                },
                "thread_id": {
                    "type": "string",
                    "description": "Thread timestamp for a thread-level override (optional)"
                },
                "mode": {
                    "type": "string",
                    "enum": ["all", "mention_only", "quiet", "default"],
                    "description": "Engagement mode to set"
                }
            },
            "required": ["channel", "mode"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: SetModeInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        let scope = EngagementScope::new(&self.agent_id, &params.channel, params.thread_id.clone());

        if params.mode == "default" {
            let existed = self.overrides.remove(&scope).await;
            self.persist_to_vault().await;
            return Ok(ToolResult::success(
                serde_json::json!({
                    "agent_id": self.agent_id,
                    "channel": params.channel,
                    "thread_id": params.thread_id,
                    "mode": "default",
                    "status": if existed { "removed" } else { "no_override" }
                }),
                start.elapsed().as_millis() as u64,
            ));
        }

        let mode = match params.mode.as_str() {
            "all" => ResponseMode::All,
            "mention_only" => ResponseMode::MentionOnly,
            "quiet" => ResponseMode::Quiet,
            other => {
                return Ok(ToolResult::failure(
                    format!(
                        "Unknown mode '{}'. Use: all, mention_only, quiet, default",
                        other
                    ),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        self.overrides.set(scope, mode, "agent").await;
        self.persist_to_vault().await;

        Ok(ToolResult::success(
            serde_json::json!({
                "agent_id": self.agent_id,
                "channel": params.channel,
                "thread_id": params.thread_id,
                "mode": params.mode,
                "status": "applied"
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ---------------------------------------------------------------------------
// messaging.get_mode
// ---------------------------------------------------------------------------

pub struct MessagingGetModeTool {
    agent_id: String,
    overrides: Arc<EngagementOverrides>,
}

impl MessagingGetModeTool {
    pub fn new(agent_id: impl Into<String>, overrides: Arc<EngagementOverrides>) -> Self {
        Self {
            agent_id: agent_id.into(),
            overrides,
        }
    }
}

#[derive(Deserialize)]
struct GetModeInput {
    channel: String,
    #[serde(default)]
    thread_id: Option<String>,
}

#[async_trait]
impl Tool for MessagingGetModeTool {
    fn name(&self) -> &str {
        "messaging.get_mode"
    }

    fn description(&self) -> &str {
        "Get this agent's current engagement mode for a channel or thread"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Channel ID to query"
                },
                "thread_id": {
                    "type": "string",
                    "description": "Thread timestamp for a thread-level query (optional)"
                }
            },
            "required": ["channel"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: GetModeInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        let resolved = self
            .overrides
            .resolve(&self.agent_id, &params.channel, params.thread_id.as_deref())
            .await;

        let output = match resolved {
            Some(mode) => serde_json::json!({
                "agent_id": self.agent_id,
                "channel": params.channel,
                "thread_id": params.thread_id,
                "mode": mode_str(&mode),
                "source": "runtime_override"
            }),
            None => serde_json::json!({
                "agent_id": self.agent_id,
                "channel": params.channel,
                "thread_id": params.thread_id,
                "mode": null,
                "source": "no_override",
                "note": "No runtime override set. Config defaults apply."
            }),
        };

        Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ---------------------------------------------------------------------------
// messaging.list_modes
// ---------------------------------------------------------------------------

pub struct MessagingListModesTool {
    agent_id: String,
    overrides: Arc<EngagementOverrides>,
}

impl MessagingListModesTool {
    pub fn new(agent_id: impl Into<String>, overrides: Arc<EngagementOverrides>) -> Self {
        Self {
            agent_id: agent_id.into(),
            overrides,
        }
    }
}

#[async_trait]
impl Tool for MessagingListModesTool {
    fn name(&self) -> &str {
        "messaging.list_modes"
    }

    fn description(&self) -> &str {
        "List this agent's runtime engagement mode overrides across channels and threads."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let entries: Vec<serde_json::Value> = self
            .overrides
            .list(Some(&self.agent_id))
            .await
            .iter()
            .map(|record| {
                serde_json::json!({
                    "channel": record.channel,
                    "thread_id": record.thread,
                    "mode": mode_str(&record.mode),
                    "set_by": record.set_by,
                    "set_at": record.set_at.to_rfc3339(),
                })
            })
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({
                "agent_id": self.agent_id,
                "overrides": entries,
                "count": entries.len()
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_mode_channel_scoped_to_agent() {
        let overrides = Arc::new(EngagementOverrides::new());
        let tool = MessagingSetModeTool::new("atlas", Arc::clone(&overrides), None);

        let result = tool
            .execute(serde_json::json!({ "channel": "C123", "mode": "all" }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["agent_id"], "atlas");
        assert_eq!(result.output["mode"], "all");
        assert_eq!(
            overrides.resolve("atlas", "C123", None).await,
            Some(ResponseMode::All)
        );
        // A different agent in the same channel is unaffected.
        assert_eq!(overrides.resolve("bernard", "C123", None).await, None);
    }

    #[tokio::test]
    async fn set_mode_thread() {
        let overrides = Arc::new(EngagementOverrides::new());
        let tool = MessagingSetModeTool::new("atlas", Arc::clone(&overrides), None);

        let result = tool
            .execute(serde_json::json!({
                "channel": "C123",
                "thread_id": "1234.5678",
                "mode": "quiet"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(
            overrides.resolve("atlas", "C123", Some("1234.5678")).await,
            Some(ResponseMode::Quiet)
        );
    }

    #[tokio::test]
    async fn set_mode_invalid() {
        let overrides = Arc::new(EngagementOverrides::new());
        let tool = MessagingSetModeTool::new("atlas", overrides, None);

        let result = tool
            .execute(serde_json::json!({ "channel": "C123", "mode": "nope" }))
            .await
            .unwrap();

        assert!(!result.success);
    }

    #[tokio::test]
    async fn get_mode_is_agent_scoped() {
        let overrides = Arc::new(EngagementOverrides::new());
        MessagingSetModeTool::new("atlas", Arc::clone(&overrides), None)
            .execute(serde_json::json!({ "channel": "C123", "mode": "all" }))
            .await
            .unwrap();

        let sable_get = MessagingGetModeTool::new("atlas", Arc::clone(&overrides));
        let result = sable_get
            .execute(serde_json::json!({ "channel": "C123" }))
            .await
            .unwrap();
        assert_eq!(result.output["mode"], "all");
        assert_eq!(result.output["source"], "runtime_override");

        // Another agent sees no override for the same channel.
        let bernard_get = MessagingGetModeTool::new("bernard", overrides);
        let result = bernard_get
            .execute(serde_json::json!({ "channel": "C123" }))
            .await
            .unwrap();
        assert!(result.output["mode"].is_null());
        assert_eq!(result.output["source"], "no_override");
    }

    #[tokio::test]
    async fn list_modes_only_shows_own_agent() {
        let overrides = Arc::new(EngagementOverrides::new());
        MessagingSetModeTool::new("atlas", Arc::clone(&overrides), None)
            .execute(serde_json::json!({ "channel": "C1", "mode": "all" }))
            .await
            .unwrap();
        MessagingSetModeTool::new("bernard", Arc::clone(&overrides), None)
            .execute(serde_json::json!({ "channel": "C2", "mode": "quiet" }))
            .await
            .unwrap();

        let tool = MessagingListModesTool::new("atlas", overrides);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"], 1);
        assert_eq!(result.output["overrides"][0]["channel"], "C1");
    }

    #[tokio::test]
    async fn set_mode_default_resets() {
        let overrides = Arc::new(EngagementOverrides::new());
        let tool = MessagingSetModeTool::new("atlas", Arc::clone(&overrides), None);

        tool.execute(serde_json::json!({ "channel": "C123", "mode": "quiet" }))
            .await
            .unwrap();
        assert_eq!(
            overrides.resolve("atlas", "C123", None).await,
            Some(ResponseMode::Quiet)
        );

        let result = tool
            .execute(serde_json::json!({ "channel": "C123", "mode": "default" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["status"], "removed");
        assert_eq!(overrides.resolve("atlas", "C123", None).await, None);
    }

    #[tokio::test]
    async fn set_mode_default_no_existing_override() {
        let overrides = Arc::new(EngagementOverrides::new());
        let tool = MessagingSetModeTool::new("atlas", overrides, None);

        let result = tool
            .execute(serde_json::json!({ "channel": "C999", "mode": "default" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["status"], "no_override");
    }
}

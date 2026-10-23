//! Agent-callable tool for adding/removing emoji reactions on messages.
//!
//! Routing semantics are shared with `messaging.*` — see
//! [`workforce_skills::adapters::messaging_routing`].

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::RwLock;
use tracing::{info, warn};

use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::channels::{ChannelSurface, Reaction, SurfaceRegistry};
use workforce_skills::adapters::messaging_routing::{
    channel_hint_from_input, invalidate_if_channel_not_found, platform_arg,
};

async fn resolve(
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

fn shared_input_schema(action: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "channel": {
                "type": "string",
                "description": format!("Channel ID where the message is (e.g. Slack channel ID)")
            },
            "message_id": {
                "type": "string",
                "description": format!("Message ID/timestamp to {action} reaction (e.g. Slack message ts)")
            },
            "emoji": {
                "type": "string",
                "description": format!("Emoji to {action}")
            },
            "platform": {
                "type": "string",
                "description": "Platform (slack, discord, etc.). Defaults to slack."
            },
            "workspace_id": {
                "type": "string",
                "description": "Workspace identifier (rarely needed — auto-detected from channel)"
            }
        },
        "required": ["channel", "message_id", "emoji"]
    })
}

fn build_reaction(input: &serde_json::Value) -> Result<Reaction> {
    Ok(Reaction {
        channel: input["channel"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'channel' parameter"))?
            .to_string(),
        message_id: input["message_id"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'message_id' parameter"))?
            .to_string(),
        emoji: input["emoji"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'emoji' parameter"))?
            .to_string(),
    })
}

pub struct ReactionTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
}

impl ReactionTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self { surfaces }
    }
}

#[async_trait]
impl Tool for ReactionTool {
    fn name(&self) -> &str {
        "reactions.add"
    }

    fn description(&self) -> &str {
        "Add an emoji reaction to a message"
    }

    fn input_schema(&self) -> serde_json::Value {
        shared_input_schema("add")
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let reaction = build_reaction(&input)?;
        let surface = resolve(&self.surfaces, &input, &reaction.channel).await?;
        let duration_ms = start.elapsed().as_millis() as u64;

        match surface.add_reaction(&reaction).await {
            Ok(()) => {
                info!(
                    emoji = %reaction.emoji,
                    channel = %reaction.channel,
                    message = %reaction.message_id,
                    platform = %surface.platform(),
                    "Reaction added"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "status": "ok",
                        "emoji": reaction.emoji,
                        "platform": surface.platform(),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(
                    error = %e,
                    emoji = %reaction.emoji,
                    channel = %reaction.channel,
                    "Failed to add reaction"
                );
                invalidate_if_channel_not_found(
                    &self.surfaces,
                    &input,
                    &reaction.channel,
                    &e.to_string(),
                )
                .await;
                Ok(ToolResult::failure(
                    format!("Failed to add reaction: {e}"),
                    duration_ms,
                ))
            }
        }
    }
}

pub struct ReactionRemoveTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
}

impl ReactionRemoveTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self { surfaces }
    }
}

#[async_trait]
impl Tool for ReactionRemoveTool {
    fn name(&self) -> &str {
        "reactions.remove"
    }

    fn description(&self) -> &str {
        "Remove a previously added emoji reaction from a message."
    }

    fn input_schema(&self) -> serde_json::Value {
        shared_input_schema("remove")
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let reaction = build_reaction(&input)?;
        let surface = resolve(&self.surfaces, &input, &reaction.channel).await?;
        let duration_ms = start.elapsed().as_millis() as u64;

        match surface.remove_reaction(&reaction).await {
            Ok(()) => Ok(ToolResult::success(
                serde_json::json!({
                    "status": "ok",
                    "emoji": reaction.emoji,
                    "removed": true,
                }),
                duration_ms,
            )),
            Err(e) => {
                warn!(error = %e, emoji = %reaction.emoji, "Failed to remove reaction");
                invalidate_if_channel_not_found(
                    &self.surfaces,
                    &input,
                    &reaction.channel,
                    &e.to_string(),
                )
                .await;
                Ok(ToolResult::failure(
                    format!("Failed to remove reaction: {e}"),
                    duration_ms,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::channels::{ChannelCapabilities, ChannelInfo, SurfaceKey};

    struct MockSurface {
        platform: String,
        workspace: String,
        reactions: Arc<tokio::sync::Mutex<Vec<Reaction>>>,
    }

    impl MockSurface {
        fn new(
            platform: &str,
            workspace: &str,
        ) -> (Arc<Self>, Arc<tokio::sync::Mutex<Vec<Reaction>>>) {
            let reactions = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    platform: platform.into(),
                    workspace: workspace.into(),
                    reactions: Arc::clone(&reactions),
                }),
                reactions,
            )
        }
    }

    #[async_trait]
    impl ChannelSurface for MockSurface {
        fn platform(&self) -> &str {
            &self.platform
        }
        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities {
                reactions: true,
                reaction_removal: true,
                channel_info: true,
                ..Default::default()
            }
        }
        async fn add_reaction(&self, reaction: &Reaction) -> Result<()> {
            self.reactions.lock().await.push(reaction.clone());
            Ok(())
        }
        async fn remove_reaction(&self, reaction: &Reaction) -> Result<()> {
            self.reactions.lock().await.push(reaction.clone());
            Ok(())
        }
        async fn channel_info(&self, channel_id: &str) -> Result<ChannelInfo> {
            Ok(ChannelInfo {
                channel_id: channel_id.into(),
                display_name: format!("#{channel_id}@{}", self.workspace),
            })
        }
    }

    fn registry_with(
        platform: &str,
        workspace: &str,
    ) -> (
        Arc<RwLock<SurfaceRegistry>>,
        Arc<tokio::sync::Mutex<Vec<Reaction>>>,
    ) {
        let (mock, recorded) = MockSurface::new(platform, workspace);
        let mut reg = SurfaceRegistry::new();
        reg.register(
            SurfaceKey::workspace(platform, workspace),
            mock as Arc<dyn ChannelSurface>,
        );
        (Arc::new(RwLock::new(reg)), recorded)
    }

    #[test]
    fn tool_definitions() {
        let surfaces = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let add_tool = ReactionTool::new(Arc::clone(&surfaces));
        let remove_tool = ReactionRemoveTool::new(surfaces);
        assert_eq!(add_tool.name(), "reactions.add");
        assert_eq!(remove_tool.name(), "reactions.remove");
        assert!(add_tool.description().contains("reaction"));
        assert!(remove_tool.description().contains("Remove"));
    }

    #[test]
    fn input_schema_has_required_fields() {
        let surfaces = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let tool = ReactionTool::new(surfaces);
        let schema = tool.input_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "channel"));
        assert!(required.iter().any(|v| v == "message_id"));
        assert!(required.iter().any(|v| v == "emoji"));
    }

    #[tokio::test]
    async fn add_reaction_dispatches_to_surface() {
        let (surfaces, recorded) = registry_with("slack", "a");
        let tool = ReactionTool::new(surfaces);
        let result = tool
            .execute(serde_json::json!({
                "channel": "C123",
                "message_id": "1234567890.000001",
                "emoji": "👍"
            }))
            .await
            .unwrap();
        assert!(result.success);
        let r = recorded.lock().await;
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].channel, "C123");
        assert_eq!(r[0].emoji, "👍");
    }

    #[tokio::test]
    async fn remove_reaction_dispatches_to_surface() {
        let (surfaces, recorded) = registry_with("slack", "a");
        let tool = ReactionRemoveTool::new(surfaces);
        let result = tool
            .execute(serde_json::json!({
                "channel": "C123",
                "message_id": "1234567890.000001",
                "emoji": "eyes"
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(recorded.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn missing_required_params_fails() {
        let (surfaces, _) = registry_with("slack", "a");
        let tool = ReactionTool::new(surfaces);
        let result = tool
            .execute(serde_json::json!({
                "message_id": "123",
                "emoji": "👍"
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn no_surface_registered_fails_gracefully() {
        let surfaces = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let tool = ReactionTool::new(surfaces);
        let result = tool
            .execute(serde_json::json!({
                "channel": "C123",
                "message_id": "123",
                "emoji": "👍"
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn explicit_workspace_routing() {
        let (mock_a, recorded_a) = MockSurface::new("slack", "a");
        let (mock_b, recorded_b) = MockSurface::new("slack", "b");
        let mut reg = SurfaceRegistry::new();
        reg.register(
            SurfaceKey::workspace("slack", "a"),
            mock_a as Arc<dyn ChannelSurface>,
        );
        reg.register(
            SurfaceKey::workspace("slack", "b"),
            mock_b as Arc<dyn ChannelSurface>,
        );
        let surfaces = Arc::new(RwLock::new(reg));
        let tool = ReactionTool::new(Arc::clone(&surfaces));

        let result = tool
            .execute(serde_json::json!({
                "channel": "C123",
                "message_id": "123",
                "emoji": "👍",
                "workspace_id": "b"
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(recorded_a.lock().await.len(), 0);
        assert_eq!(recorded_b.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn unknown_platform_fails() {
        let surfaces = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let tool = ReactionTool::new(surfaces);
        let result = tool
            .execute(serde_json::json!({
                "channel": "C123",
                "message_id": "123",
                "emoji": "👍",
                "platform": "nonexistent"
            }))
            .await;
        assert!(result.is_err());
    }
}

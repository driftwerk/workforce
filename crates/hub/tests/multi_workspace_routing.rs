//! End-to-end regression test for the multi-workspace Slack routing fix.
//!
//! The pre-fix bug: when an agent had bots in two Slack workspaces and was
//! asked to read a channel that existed only in the second workspace, the
//! `messaging.read` tool would silently pick the first workspace's bot
//! and Slack would return `channel_not_found` because that bot wasn't in
//! the channel. The fix routes channel operations to the workspace whose
//! `conversations.info` probe succeeds.
//!
//! This test wires up two mock `ChannelSurface`s and asserts the right
//! one handles each call without the agent having to specify a workspace.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use tokio::sync::RwLock;

use workforce_agent::tools::Tool;
use workforce_common::channels::{
    ChannelCapabilities, ChannelInfo, ChannelMessage, ChannelSurface, OutboundMessage, Reaction,
    SentMessage, SurfaceKey, SurfaceRegistry,
};
use workforce_skills::adapters::messaging::{MessagingReadTool, MessagingSendTool};

struct PinnedSurface {
    workspace: String,
    known_channels: std::collections::HashSet<String>,
    reads: Arc<AtomicUsize>,
    sends: Arc<AtomicUsize>,
}

impl PinnedSurface {
    fn arc(workspace: &str, channels: &[&str]) -> (Arc<Self>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let reads = Arc::new(AtomicUsize::new(0));
        let sends = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                workspace: workspace.into(),
                known_channels: channels.iter().map(|s| s.to_string()).collect(),
                reads: Arc::clone(&reads),
                sends: Arc::clone(&sends),
            }),
            reads,
            sends,
        )
    }
}

#[async_trait]
impl ChannelSurface for PinnedSurface {
    fn platform(&self) -> &str {
        "slack"
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
        if self.known_channels.contains(channel_id) {
            Ok(ChannelInfo {
                channel_id: channel_id.into(),
                display_name: format!("#{channel_id}@{}", self.workspace),
            })
        } else {
            Err(eyre!("channel_not_found"))
        }
    }
    async fn send_message(&self, message: &OutboundMessage) -> Result<SentMessage> {
        if !self.known_channels.contains(&message.channel) {
            return Err(eyre!("channel_not_found"));
        }
        self.sends.fetch_add(1, Ordering::SeqCst);
        Ok(SentMessage {
            channel: message.channel.clone(),
            message_id: format!("{}-{}", self.workspace, message.text.len()),
            thread_id: message.thread_id.clone(),
            platform: "slack".into(),
        })
    }
    async fn read_messages(
        &self,
        channel: &str,
        _limit: usize,
        _oldest: Option<&str>,
    ) -> Result<Vec<ChannelMessage>> {
        if !self.known_channels.contains(channel) {
            return Err(eyre!("channel_not_found"));
        }
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(vec![ChannelMessage {
            channel: channel.into(),
            message_id: format!("ts-{}", self.workspace),
            user: "U1".into(),
            user_name: None,
            channel_name: None,
            text: format!("hello from {}", self.workspace),
            timestamp: "1.0".into(),
            thread_id: None,
            files: vec![],
            bot_id: None,
        }])
    }
}

fn build_registry() -> (Arc<RwLock<SurfaceRegistry>>, MockHandles) {
    let (sable_surface, sable_reads, sable_sends) =
        PinnedSurface::arc("atlas", &["C_SABLE_GENERAL"]);
    let (candy_surface, candy_reads, candy_sends) = PinnedSurface::arc("candy", &["C0FAKECHAN1"]);

    let mut reg = SurfaceRegistry::new();
    reg.register(
        SurfaceKey::workspace("slack", "atlas"),
        sable_surface as Arc<dyn ChannelSurface>,
    );
    reg.register(
        SurfaceKey::workspace("slack", "candy"),
        candy_surface as Arc<dyn ChannelSurface>,
    );

    (
        Arc::new(RwLock::new(reg)),
        MockHandles {
            sable_reads,
            sable_sends,
            candy_reads,
            candy_sends,
        },
    )
}

struct MockHandles {
    sable_reads: Arc<AtomicUsize>,
    sable_sends: Arc<AtomicUsize>,
    candy_reads: Arc<AtomicUsize>,
    candy_sends: Arc<AtomicUsize>,
}

#[tokio::test]
async fn pre_fix_bug_reproduction_now_succeeds() {
    let (surfaces, handles) = build_registry();
    let tool = MessagingReadTool::new(surfaces);

    let result = tool
        .execute(serde_json::json!({
            "channel": "C0FAKECHAN1"
        }))
        .await
        .expect("tool should not error at the dispatch layer");

    assert!(
        result.success,
        "messaging.read should succeed by routing to the candy workspace; got: {:?}",
        result.error
    );
    assert_eq!(handles.candy_reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        handles.sable_reads.load(Ordering::SeqCst),
        0,
        "atlas bot must not be used for a candy-owned channel"
    );
    assert_eq!(result.output["count"], 1);
}

#[tokio::test]
async fn explicit_workspace_overrides_routing() {
    let (surfaces, handles) = build_registry();
    let tool = MessagingReadTool::new(surfaces);

    let result = tool
        .execute(serde_json::json!({
            "channel": "C_SABLE_GENERAL",
            "workspace_id": "atlas"
        }))
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(handles.sable_reads.load(Ordering::SeqCst), 1);
    assert_eq!(handles.candy_reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn send_message_routes_to_owning_workspace() {
    let (surfaces, handles) = build_registry();
    let tool = MessagingSendTool::new(surfaces);

    let result = tool
        .execute(serde_json::json!({
            "channel": "C0FAKECHAN1",
            "text": "hello"
        }))
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(handles.candy_sends.load(Ordering::SeqCst), 1);
    assert_eq!(handles.sable_sends.load(Ordering::SeqCst), 0);
    assert!(result.output["message_id"]
        .as_str()
        .unwrap()
        .starts_with("candy-"));
}

#[tokio::test]
async fn unreachable_channel_returns_descriptive_error() {
    let (surfaces, handles) = build_registry();
    let tool = MessagingReadTool::new(surfaces);

    let err = tool
        .execute(serde_json::json!({
            "channel": "C_NONEXISTENT"
        }))
        .await
        .expect_err("dispatch must error when no workspace owns the channel");
    let msg = err.to_string();
    assert!(
        msg.contains("atlas") && msg.contains("candy"),
        "error must name both probed workspaces: {msg}"
    );
    assert_eq!(handles.sable_reads.load(Ordering::SeqCst), 0);
    assert_eq!(handles.candy_reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn channel_not_found_invalidates_cached_route() {
    let (surfaces, _) = build_registry();
    surfaces
        .read()
        .await
        .routes()
        .insert("slack", "C0FAKECHAN1", "atlas")
        .await;

    let tool = MessagingReadTool::new(Arc::clone(&surfaces));
    let result = tool
        .execute(serde_json::json!({"channel": "C0FAKECHAN1"}))
        .await
        .unwrap();
    assert!(
        !result.success,
        "stale cache routes to atlas, which doesn't own the channel — expected failure"
    );

    let cached = surfaces
        .read()
        .await
        .routes()
        .lookup("slack", "C0FAKECHAN1")
        .await;
    assert!(
        cached.is_none(),
        "channel_not_found must invalidate the stale cache entry"
    );

    let retry = tool
        .execute(serde_json::json!({"channel": "C0FAKECHAN1"}))
        .await
        .unwrap();
    assert!(
        retry.success,
        "after invalidation, probe must succeed via candy"
    );
    assert_eq!(
        surfaces
            .read()
            .await
            .routes()
            .lookup("slack", "C0FAKECHAN1")
            .await
            .as_deref(),
        Some("candy"),
    );
}

#[tokio::test]
async fn subsequent_calls_reuse_cached_route() {
    let (surfaces, _handles) = build_registry();
    let tool = MessagingReadTool::new(Arc::clone(&surfaces));

    for _ in 0..3 {
        let result = tool
            .execute(serde_json::json!({"channel": "C0FAKECHAN1"}))
            .await
            .unwrap();
        assert!(result.success);
    }

    let cached = surfaces
        .read()
        .await
        .routes()
        .lookup("slack", "C0FAKECHAN1")
        .await;
    assert_eq!(cached.as_deref(), Some("candy"));
}

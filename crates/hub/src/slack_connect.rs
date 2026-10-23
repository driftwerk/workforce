//! ChatGPT sign-in codes on Slack.
//!
//! A code binds whoever enters it to the person's login, so it goes to them
//! alone: inline only in their direct message with the agent, else by a
//! direct message of its own, the thread told once it was sent. Each message
//! that carried a code is kept on the flow and settled in place when the
//! flow ends, so no live-looking code outlasts it.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use uuid::Uuid;
use workforce_common::channels::{ChannelSurface, OutboundMessage};
use workforce_common::session::SessionContextType;
use workforce_common::subscription::{ConnectPrompt, SubscriptionKind};

use crate::agent_registry::AgentRegistry;
use crate::hub_core::logins::device_login::{
    DeviceFlow, DeviceLoginListener, DeviceLogins, FlowState, SlackDelivery, CODE_WARNING,
};
use crate::serve::AgentService;
use crate::slack_manager::WorkspaceSurfaces;

/// The sign-in button's action; Slack opens its URL, and the click needs no
/// answer from the hub.
pub const OPEN_ACTION: &str = "subscription_connect_open";

/// The person a Slack turn ran for: their Slack id, and the identity it
/// resolved to.
pub struct Requester<'a> {
    pub slack_id: &'a str,
    pub identity: Option<&'a str>,
}

/// Hand the code of the sign-in `prompt` started to the person it is for:
/// inline in their DM with the agent, else by DM, the thread then told so.
pub async fn deliver_code(
    service: &AgentService,
    surface: &Arc<dyn ChannelSurface>,
    workspace_id: &str,
    (channel, thread): (&str, Option<&str>),
    requester: Requester<'_>,
    session_id: &str,
    prompt: &ConnectPrompt,
) {
    let (SubscriptionKind::ChatGpt, Some(flow_id)) = (prompt.kind, prompt.flow_id) else {
        return;
    };
    let Some(identity) = requester
        .identity
        .and_then(|identity| Uuid::parse_str(identity).ok())
    else {
        return;
    };
    let device = service.subscriptions().device_logins();
    let Some(flow) = device
        .get(identity, flow_id)
        .filter(|flow| flow.state.is_pending())
    else {
        return;
    };
    let in_dm = service
        .sessions
        .get(session_id)
        .ok()
        .flatten()
        .is_some_and(|session| session.context_type == Some(SessionContextType::Dm));
    let sent = match in_dm {
        true => send_code(surface, workspace_id, channel, thread, &flow).await,
        false => match surface.open_conversation(requester.slack_id).await {
            Ok(dm) => send_code(surface, workspace_id, &dm, None, &flow).await,
            Err(e) => Err(e),
        },
    };
    let delivery = match sent {
        Ok(delivery) => delivery,
        Err(e) => {
            tracing::warn!(error = %e, flow = %flow.id, "the sign-in code was not delivered");
            return;
        }
    };
    if !in_dm {
        let told = surface
            .send_message(&OutboundMessage {
                thread_id: thread.map(str::to_string),
                ..OutboundMessage::text(
                    channel,
                    format!(
                        "<@{}>, I've sent you a ChatGPT sign-in code by direct message.",
                        requester.slack_id
                    ),
                )
            })
            .await;
        if let Err(e) = told {
            tracing::warn!(error = %e, "the thread was not told a sign-in code was sent");
        }
    }
    if let Err(e) = device.note_delivery(identity, flow.id, delivery).await {
        tracing::warn!(error = %e, flow = %flow.id, "the code's message was not kept for settling");
    }
}

async fn send_code(
    surface: &Arc<dyn ChannelSurface>,
    workspace_id: &str,
    channel: &str,
    thread: Option<&str>,
    flow: &DeviceFlow,
) -> eyre::Result<SlackDelivery> {
    let expires = flow.expires_at.format("%H:%M UTC");
    let text = format!(
        "Your ChatGPT sign-in code is {} (expires {expires}). Enter it at {}. {CODE_WARNING}",
        flow.user_code, flow.verification_url
    );
    let blocks = vec![serde_json::json!({
        "type": "section",
        "text": {
            "type": "mrkdwn",
            "text": format!(
                "*Sign in to ChatGPT*\nYour one-time code: `{}` (expires {expires})\n_{CODE_WARNING}_",
                flow.user_code
            ),
        },
        "accessory": {
            "type": "button",
            "text": { "type": "plain_text", "text": "Open the sign-in page" },
            "url": flow.verification_url,
            "action_id": OPEN_ACTION,
        },
    })];
    let sent = surface
        .send_message(&OutboundMessage {
            thread_id: thread.map(str::to_string),
            blocks: Some(blocks),
            ..OutboundMessage::text(channel, text)
        })
        .await?;
    Ok(SlackDelivery {
        workspace_id: workspace_id.to_string(),
        channel: sent.channel,
        ts: sent.message_id,
    })
}

/// What a code's message becomes once its flow ends; `reruns` is whether a
/// message that waited on it is answered now.
fn settled_text(flow: &DeviceFlow, reruns: bool) -> String {
    match &flow.state {
        FlowState::Completed { account } => {
            let who = [account.email.as_deref(), account.plan.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ");
            let connected = match who.is_empty() {
                true => "✅ ChatGPT connected.".to_string(),
                false => format!("✅ ChatGPT connected ({who})."),
            };
            match reruns {
                true => format!("{connected} Re-running your message."),
                false => connected,
            }
        }
        FlowState::Expired => "⌛ Code expired; send your message again.".to_string(),
        FlowState::Failed { reason } => format!("The ChatGPT sign-in failed: {reason}"),
        _ => "This ChatGPT sign-in code is no longer in use.".to_string(),
    }
}

/// Settles every message that carried a flow's code when the flow ends, in
/// the workspace it was sent in.
struct SlackSettle {
    surfaces: WorkspaceSurfaces,
    registry: Weak<AgentRegistry>,
}

#[async_trait]
impl DeviceLoginListener for SlackSettle {
    async fn settled(&self, flow: DeviceFlow) {
        let reruns = self
            .registry
            .upgrade()
            .is_some_and(|registry| crate::agent_turn::connect::reruns_any(&registry, &flow));
        let text = settled_text(&flow, reruns);
        for delivery in &flow.deliveries {
            let Some(surface) = self.surfaces.get(&delivery.workspace_id) else {
                tracing::warn!(flow = %flow.id, workspace = %delivery.workspace_id, "a sign-in code's message was not settled: its workspace is gone");
                continue;
            };
            if let Err(e) = surface
                .update_message(
                    &delivery.channel,
                    &delivery.ts,
                    &OutboundMessage {
                        blocks: Some(vec![serde_json::json!({
                            "type": "section",
                            "text": { "type": "mrkdwn", "text": text },
                        })]),
                        ..OutboundMessage::text(&delivery.channel, text.clone())
                    },
                )
                .await
            {
                tracing::warn!(error = %e, flow = %flow.id, "a sign-in code's message was not settled");
            }
        }
    }
}

/// Settle the Slack messages that carry `device`'s codes when their flows
/// end.
pub fn settle_codes(
    device: &DeviceLogins,
    registry: &Arc<AgentRegistry>,
    surfaces: WorkspaceSurfaces,
) {
    device.on_complete(Arc::new(SlackSettle {
        surfaces,
        registry: Arc::downgrade(registry),
    }));
}

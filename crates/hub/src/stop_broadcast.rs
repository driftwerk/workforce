//! Cross-instance stop propagation.
//!
//! A "stop" typed in Slack lands on the instance holding the Slack sockets,
//! but during a rolling deploy (or with standing replicas) the work it targets
//! may be draining on another instance. The originator fans the stop out over
//! plain NATS (every instance subscribes — no queue group) and waits briefly
//! for a positive reply, so the ack it posts is deterministic: either some
//! instance actually cancelled work, or nothing was running anywhere and the
//! message falls through to a normal turn.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::hosts::RegistryView;

const STOP_SUBJECT: &str = "workforce.hub.stop";

/// How long the originator waits for an instance to report a cancellation.
/// Cancels are token flips + map scans — replies arrive in milliseconds; the
/// window only pads for NATS round-trips.
const REPLY_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, Serialize, Deserialize)]
struct StopRequest {
    agent_id: String,
    channel: String,
    thread: Option<String>,
    /// The publisher's instance id, so it can ignore its own broadcast — it
    /// has already cancelled (or found nothing) locally.
    origin: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StopReply {
    cancelled: usize,
}

/// Fan a stop out to every hub instance and report whether any of them
/// cancelled work in scope. `false` also covers NATS errors and silence —
/// the caller then falls through to the status-quo behaviour.
pub(crate) async fn broadcast_stop(
    nats: &async_nats::Client,
    agent_id: &str,
    channel: &str,
    thread: Option<&str>,
) -> bool {
    let request = StopRequest {
        agent_id: agent_id.to_string(),
        channel: channel.to_string(),
        thread: thread.map(str::to_string),
        origin: workforce_common::instance::process_instance_id().to_string(),
    };
    let payload = match serde_json::to_vec(&request) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "Encoding stop broadcast failed");
            return false;
        }
    };
    let inbox = nats.new_inbox();
    let mut replies = match nats.subscribe(inbox.clone()).await {
        Ok(sub) => sub,
        Err(e) => {
            warn!(error = %e, "Subscribing for stop-broadcast replies failed");
            return false;
        }
    };
    if let Err(e) = nats
        .publish_with_reply(STOP_SUBJECT, inbox, payload.into())
        .await
    {
        warn!(error = %e, "Publishing stop broadcast failed");
        return false;
    }

    let deadline = tokio::time::Instant::now() + REPLY_WAIT;
    loop {
        let msg = match tokio::time::timeout_at(deadline, replies.next()).await {
            Ok(Some(msg)) => msg,
            Ok(None) | Err(_) => return false,
        };
        match serde_json::from_slice::<StopReply>(&msg.payload) {
            Ok(reply) if reply.cancelled > 0 => return true,
            Ok(_) => {}
            Err(e) => warn!(error = %e, "Undecodable stop-broadcast reply"),
        }
    }
}

/// Subscribe this instance to stop broadcasts and cancel matching local work.
/// Runs on every instance for the whole process lifetime.
pub fn spawn_stop_listener(
    nats: async_nats::Client,
    registry: Arc<dyn RegistryView>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut sub = match nats.subscribe(STOP_SUBJECT).await {
            Ok(sub) => sub,
            Err(e) => {
                warn!(error = %e, "Subscribing to stop broadcasts failed");
                return;
            }
        };
        while let Some(msg) = sub.next().await {
            let request: StopRequest = match serde_json::from_slice(&msg.payload) {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "Undecodable stop broadcast");
                    continue;
                }
            };
            if request.origin == workforce_common::instance::process_instance_id() {
                continue;
            }
            let Some(svc) = registry.get(&request.agent_id) else {
                continue;
            };
            let cancelled = crate::slack_manager::cancel_scoped_work(
                &svc,
                &request.channel,
                request.thread.as_deref(),
            )
            .await;
            // Reply before settling: the originator's wait window is short,
            // and the teardown wait + session annotation can take seconds.
            if let Some(reply_to) = msg.reply {
                let reply = StopReply {
                    cancelled: cancelled.total(),
                };
                if let Ok(payload) = serde_json::to_vec(&reply) {
                    let _ = nats.publish(reply_to, payload.into()).await;
                }
            }
            if cancelled.total() > 0 {
                info!(
                    agent = %request.agent_id,
                    channel = %request.channel,
                    cancelled = cancelled.total(),
                    "Stop broadcast — cancelled local work for another instance's stop"
                );
                // Settle off the subscription loop: the teardown wait can take
                // seconds and must not delay the next stop broadcast.
                tokio::spawn(cancelled.settle());
            }
        }
    })
}

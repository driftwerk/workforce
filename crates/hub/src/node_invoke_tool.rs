//! Node Invoke Tool — lets agents invoke device capabilities on connected clients.
//!
//! When an agent needs access to a device capability (camera, location, notifications, etc.),
//! it calls this tool. The tool sends a `node_invoke` request over the broadcast channel to
//! the WebSocket clients of the person the turn runs as. The device client executes the
//! command and sends back a `node_invoke_result`, which is matched by `invoke_id` and
//! returned to the agent when it comes from that same person's connection.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use tokio::sync::{broadcast, RwLock};
use tracing::{info, warn};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_gateway_client::protocol::NodeInvokeResult;

use crate::broadcast::BroadcastMessage;

/// The invocations awaiting a device's answer, by invoke id, each with the
/// person whose turn asked: a camera or a location is that person's, so only
/// their devices are asked and only their connections may answer.
#[derive(Default)]
pub struct PendingInvokes(RwLock<HashMap<String, PendingInvoke>>);

struct PendingInvoke {
    owner: String,
    reply: tokio::sync::oneshot::Sender<NodeInvokeResult>,
}

impl PendingInvokes {
    async fn expect(
        &self,
        invoke_id: &str,
        owner: &str,
    ) -> tokio::sync::oneshot::Receiver<NodeInvokeResult> {
        let (reply, answered) = tokio::sync::oneshot::channel();
        self.0.write().await.insert(
            invoke_id.to_string(),
            PendingInvoke {
                owner: owner.to_string(),
                reply,
            },
        );
        answered
    }

    async fn forget(&self, invoke_id: &str) {
        self.0.write().await.remove(invoke_id);
    }

    /// Hand `result` to the call awaiting it when it comes from a connection
    /// of the person that call was made for (`sender`); anyone else's answer
    /// is refused and the call keeps waiting for its own device.
    pub async fn fulfil(&self, sender: Option<&str>, result: NodeInvokeResult) -> bool {
        let mut pending = self.0.write().await;
        let theirs = pending
            .get(&result.invoke_id)
            .is_some_and(|invoke| sender == Some(invoke.owner.as_str()));
        theirs
            && pending
                .remove(&result.invoke_id)
                .is_some_and(|invoke| invoke.reply.send(result).is_ok())
    }
}

/// Agent tool that invokes a device capability on a connected node (iOS/Android/desktop client).
///
/// Supported commands:
/// - `camera.snap` — capture a photo from the device camera
/// - `camera.list` — list available cameras
/// - `location.get` — get the device's current GPS location
/// - `system.notify` — send a local notification on the device
pub struct NodeInvokeTool {
    broadcast_tx: broadcast::Sender<BroadcastMessage>,
    node_invoke_pending: Arc<PendingInvokes>,
}

impl NodeInvokeTool {
    pub fn new(
        broadcast_tx: broadcast::Sender<BroadcastMessage>,
        node_invoke_pending: Arc<PendingInvokes>,
    ) -> Self {
        Self {
            broadcast_tx,
            node_invoke_pending,
        }
    }
}

#[derive(Deserialize)]
struct NodeInvokeInput {
    command: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for NodeInvokeTool {
    fn name(&self) -> &str {
        "node_invoke"
    }

    fn description(&self) -> &str {
        "Invoke capability on a connected device (camera, notify, etc)"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The device capability command to invoke (e.g",
                    "enum": ["camera.snap", "camera.list", "camera.clip", "location.get", "system.notify"]
                },
                "params": {
                    "type": "object",
                    "description": "Command-specific parameters"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 15)",
                    "default": 15
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: NodeInvokeInput = serde_json::from_value(input)
            .map_err(|e| eyre::eyre!("Invalid node_invoke input: {}", e))?;

        let invoke_id = uuid::Uuid::new_v4().to_string();
        let timeout = Duration::from_secs(input.timeout_secs.unwrap_or(15));

        info!(
            invoke_id = %invoke_id,
            command = %input.command,
            "Sending node invoke request to connected device"
        );

        // Build the invoke request payload that the client will parse from the content field.
        let request_payload = serde_json::json!({
            "invoke_id": invoke_id,
            "command": input.command,
            "params": input.params,
            "timeout_ms": timeout.as_millis() as u64,
        });
        let content = serde_json::to_string(&request_payload)?;

        let Some(owner) = crate::user_config::current_identity_id() else {
            return Ok(ToolResult::failure(
                "A device belongs to a person, and this turn runs as nobody.",
                start.elapsed().as_millis() as u64,
            ));
        };

        // Expect the answer before broadcasting (avoids race condition).
        let rx = self.node_invoke_pending.expect(&invoke_id, &owner).await;

        // Session-less, so it reaches the person's device whichever
        // conversation the turn runs in — and nobody else's.
        let send_result = self
            .broadcast_tx
            .send(BroadcastMessage::node_invoke(&owner, content));

        if send_result.is_err() {
            // No receivers — no clients connected.
            self.node_invoke_pending.forget(&invoke_id).await;
            return Ok(ToolResult::failure(
                "No device clients connected. The user needs to connect a device (e.g. the Driftwerk iOS app) to use this capability.",
                start.elapsed().as_millis() as u64,
            ));
        }

        // Wait for the device to respond.
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => {
                let elapsed = start.elapsed().as_millis() as u64;
                if result.ok {
                    info!(
                        invoke_id = %result.invoke_id,
                        command = %input.command,
                        duration_ms = elapsed,
                        "Node invoke succeeded"
                    );
                    Ok(ToolResult::success(
                        result.payload.unwrap_or(serde_json::Value::Null),
                        elapsed,
                    ))
                } else {
                    let error = result
                        .error
                        .unwrap_or_else(|| "Unknown device error".to_string());
                    warn!(
                        invoke_id = %result.invoke_id,
                        command = %input.command,
                        error = %error,
                        "Node invoke failed"
                    );
                    Ok(ToolResult::failure(error, elapsed))
                }
            }
            Ok(Err(_)) => {
                // Oneshot sender was dropped (connection lost).
                self.node_invoke_pending.forget(&invoke_id).await;
                Ok(ToolResult::failure(
                    "Device disconnected before responding to the invoke request.",
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(_) => {
                // Timeout — clean up the pending entry.
                self.node_invoke_pending.forget(&invoke_id).await;
                warn!(
                    invoke_id = %invoke_id,
                    command = %input.command,
                    timeout_secs = timeout.as_secs(),
                    "Node invoke timed out"
                );
                Ok(ToolResult::failure(
                    format!(
                        "Device did not respond within {} seconds. The device may not support the '{}' command, or the app may be in the background.",
                        timeout.as_secs(),
                        input.command
                    ),
                    start.elapsed().as_millis() as u64,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(invoke_id: &str) -> NodeInvokeResult {
        serde_json::from_value(serde_json::json!({
            "type": "node_invoke_result",
            "invoke_id": invoke_id,
            "ok": true,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn only_the_person_a_device_was_asked_for_answers_the_call() {
        let pending = PendingInvokes::default();
        let mut answered = pending.expect("i1", "alice").await;

        assert!(!pending.fulfil(Some("bob"), result("i1")).await);
        assert!(!pending.fulfil(None, result("i1")).await);
        assert!(!pending.fulfil(Some("alice"), result("unknown")).await);
        assert!(answered.try_recv().is_err(), "the call is still waiting");

        assert!(pending.fulfil(Some("alice"), result("i1")).await);
        assert_eq!(answered.await.unwrap().invoke_id, "i1");
        assert!(
            !pending.fulfil(Some("alice"), result("i1")).await,
            "answered once"
        );
    }
}

//! Local Agent Registry — tracks connected local agents and their communication channels.
//!
//! Each local agent connects via `/ws/agent`, sends a `local_agent_hello` handshake,
//! and is registered here. The gateway can then route `exec.run` tool calls to the
//! local agent instead of executing them on the server.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{eyre, Result};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{info, warn};

use workforce_gateway_client::protocol::{LocalExecCancel, LocalExecRequest, LocalExecResult};

/// Info about a connected local agent (for API/display).
#[derive(Debug, Clone, Serialize)]
pub struct LocalAgentInfo {
    pub id: String,
    pub name: String,
    pub capabilities: Vec<String>,
    pub connected_at: DateTime<Utc>,
}

/// A connected local agent with its communication channels.
pub struct ConnectedLocalAgent {
    pub id: String,
    pub name: String,
    pub capabilities: Vec<String>,
    pub connected_at: DateTime<Utc>,
    /// Send serialized JSON messages to this agent's WebSocket write loop.
    pub request_tx: mpsc::Sender<String>,
    /// Pending exec results, keyed by request_id.
    pub pending: Arc<RwLock<HashMap<String, oneshot::Sender<LocalExecResult>>>>,
}

/// Registry of connected local agents.
pub struct LocalAgentRegistry {
    agents: RwLock<HashMap<String, ConnectedLocalAgent>>,
}

impl Default for LocalAgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalAgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
        }
    }

    /// Register a newly connected local agent.
    pub async fn register(
        &self,
        id: String,
        name: String,
        capabilities: Vec<String>,
        request_tx: mpsc::Sender<String>,
    ) {
        info!(agent_id = %id, name = %name, "Registering local agent");
        let agent = ConnectedLocalAgent {
            id: id.clone(),
            name,
            capabilities,
            connected_at: Utc::now(),
            request_tx,
            pending: Arc::new(RwLock::new(HashMap::new())),
        };
        self.agents.write().await.insert(id, agent);
    }

    /// Deregister a local agent (on disconnect).
    pub async fn deregister(&self, id: &str) {
        if let Some(agent) = self.agents.write().await.remove(id) {
            info!(agent_id = %id, name = %agent.name, "Deregistered local agent");
            // Drop all pending oneshot senders so waiters get an error
            let mut pending = agent.pending.write().await;
            let count = pending.len();
            pending.clear();
            if count > 0 {
                warn!(
                    agent_id = %id,
                    pending_count = count,
                    "Dropped pending exec requests for disconnected agent"
                );
            }
        }
    }

    /// Get display info for a connected agent.
    pub async fn get_info(&self, id: &str) -> Option<LocalAgentInfo> {
        let agents = self.agents.read().await;
        agents.get(id).map(|a| LocalAgentInfo {
            id: a.id.clone(),
            name: a.name.clone(),
            capabilities: a.capabilities.clone(),
            connected_at: a.connected_at,
        })
    }

    /// Check if an agent is currently connected.
    pub async fn is_connected(&self, id: &str) -> bool {
        self.agents.read().await.contains_key(id)
    }

    /// List all connected local agents.
    pub async fn list(&self) -> Vec<LocalAgentInfo> {
        self.agents
            .read()
            .await
            .values()
            .map(|a| LocalAgentInfo {
                id: a.id.clone(),
                name: a.name.clone(),
                capabilities: a.capabilities.clone(),
                connected_at: a.connected_at,
            })
            .collect()
    }

    /// Send an exec request to a local agent and return a receiver for the result.
    ///
    /// This follows the oneshot request/response pattern from `NodeInvokeTool`:
    /// 1. Look up the agent in the registry
    /// 2. Create a oneshot channel
    /// 3. Store the sender in `agent.pending` keyed by `request.request_id`
    /// 4. Serialize the request to JSON and send via `agent.request_tx`
    /// 5. Return the receiver
    pub async fn send_exec(
        &self,
        id: &str,
        request: LocalExecRequest,
    ) -> Result<oneshot::Receiver<LocalExecResult>> {
        let agents = self.agents.read().await;
        let agent = agents
            .get(id)
            .ok_or_else(|| eyre!("Local agent '{}' not connected", id))?;

        let (tx, rx) = oneshot::channel();

        // Store the oneshot sender keyed by request_id
        {
            let mut pending = agent.pending.write().await;
            pending.insert(request.request_id.clone(), tx);
        }

        // Serialize and send the request to the agent's WS write loop
        let json = serde_json::to_string(&request)?;
        agent.request_tx.send(json).await.map_err(|_| {
            eyre!(
                "Failed to send request to local agent '{}' — channel closed",
                id
            )
        })?;

        info!(
            agent_id = %id,
            request_id = %request.request_id,
            command = %request.command,
            "Sent exec request to local agent"
        );

        Ok(rx)
    }

    /// Send a cancel message to a local agent.
    pub async fn send_cancel(&self, id: &str, request_id: &str) -> Result<()> {
        let agents = self.agents.read().await;
        let agent = agents
            .get(id)
            .ok_or_else(|| eyre!("Local agent '{}' not connected", id))?;

        let cancel = LocalExecCancel {
            msg_type: "local_exec_cancel".to_string(),
            request_id: request_id.to_string(),
        };

        let json = serde_json::to_string(&cancel)?;
        agent.request_tx.send(json).await.map_err(|_| {
            eyre!(
                "Failed to send cancel to local agent '{}' — channel closed",
                id
            )
        })?;

        info!(
            agent_id = %id,
            request_id = %request_id,
            "Sent cancel to local agent"
        );

        Ok(())
    }

    /// Fulfill a pending exec result (called when WS handler receives `local_exec_result`).
    pub async fn fulfill_result(&self, agent_id: &str, result: LocalExecResult) {
        let agents = self.agents.read().await;
        if let Some(agent) = agents.get(agent_id) {
            let mut pending = agent.pending.write().await;
            if let Some(sender) = pending.remove(&result.request_id) {
                let request_id = result.request_id.clone();
                if sender.send(result).is_err() {
                    warn!(
                        agent_id = %agent_id,
                        request_id = %request_id,
                        "Failed to fulfill exec result — receiver dropped"
                    );
                }
            } else {
                warn!(
                    agent_id = %agent_id,
                    request_id = %result.request_id,
                    "No pending request for exec result"
                );
            }
        } else {
            warn!(
                agent_id = %agent_id,
                "Received exec result for unknown agent"
            );
        }
    }
}

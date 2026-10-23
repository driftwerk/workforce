//! Local Agent Exec Tool — routes `exec.run` calls to a connected local agent.
//!
//! When a session has a `selected_local_agent` in its metadata, this tool replaces
//! the default `exec.run` implementation. Commands are sent to the local agent's
//! machine via WebSocket, with identity-file safety (DW-327) enforced: any command
//! touching protected paths is routed to the gateway instead.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use tracing::{info, warn};

use workforce_agent::tools::{Tool, ToolResult};
use workforce_gateway_client::protocol::LocalExecRequest;

use crate::local_agent_registry::LocalAgentRegistry;

/// Protected paths that must always execute on the gateway (DW-327).
///
/// Commands referencing these identity/memory files are never delegated to a
/// local agent — they always run on the gateway where the canonical files live.
const PROTECTED_PATTERNS: &[&str] = &[
    "SOUL.md",
    "AGENTS.md",
    "TOOLS.md",
    "IDENTITY.md",
    "HEARTBEAT.md",
    "WELCOME.md",
    "/memory/",
    "/notes/",
    "/.local/",
];

/// Check whether a command references a protected path.
fn is_protected_path(command: &str) -> bool {
    PROTECTED_PATTERNS.iter().any(|p| command.contains(p))
}

/// Tool implementation that routes `exec.run` to a connected local agent,
/// falling back to the gateway's own exec tool when the agent is unavailable
/// or the command touches protected identity files.
pub struct LocalAgentExecTool {
    local_agents: Arc<LocalAgentRegistry>,
    target_agent_id: String,
    fallback_exec: Arc<dyn Tool>,
}

impl LocalAgentExecTool {
    pub fn new(
        local_agents: Arc<LocalAgentRegistry>,
        target_agent_id: String,
        fallback_exec: Arc<dyn Tool>,
    ) -> Self {
        Self {
            local_agents,
            target_agent_id,
            fallback_exec,
        }
    }
}

#[derive(Deserialize)]
struct ExecInput {
    command: String,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for LocalAgentExecTool {
    fn name(&self) -> &str {
        "exec.run"
    }

    fn description(&self) -> &str {
        "Manage local agent processes"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" },
                "workdir": { "type": "string", "description": "Working directory (optional)" },
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds (optional)" }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let exec_input: ExecInput = serde_json::from_value(input.clone())
            .map_err(|e| eyre::eyre!("Invalid exec.run input: {}", e))?;

        // DW-327: Identity files always execute on gateway
        if is_protected_path(&exec_input.command) {
            info!(
                command = %exec_input.command,
                "Protected path — routing to gateway"
            );
            return self.fallback_exec.execute(input).await;
        }

        // Build the request for the local agent
        let request_id = uuid::Uuid::new_v4().to_string();
        let request = LocalExecRequest {
            msg_type: "local_exec".to_string(),
            request_id: request_id.clone(),
            command: exec_input.command.clone(),
            workdir: exec_input.workdir.clone(),
            timeout_secs: exec_input.timeout_secs,
        };

        // Route to local agent
        match self
            .local_agents
            .send_exec(&self.target_agent_id, request)
            .await
        {
            Ok(rx) => {
                let timeout = Duration::from_secs(exec_input.timeout_secs.unwrap_or(300));
                match tokio::time::timeout(timeout, rx).await {
                    Ok(Ok(result)) => {
                        let elapsed = start.elapsed().as_millis() as u64;
                        if result.ok {
                            info!(
                                agent = %self.target_agent_id,
                                request_id = %request_id,
                                duration_ms = elapsed,
                                "Local exec succeeded"
                            );
                            // Build output matching the standard exec.run format
                            let output = serde_json::json!({
                                "stdout": result.stdout.unwrap_or_default(),
                                "stderr": result.stderr.unwrap_or_default(),
                                "exit_code": result.exit_code.unwrap_or(0),
                                "source": "local_agent",
                                "agent_id": self.target_agent_id,
                            });
                            Ok(ToolResult::success(output, elapsed))
                        } else {
                            let error = result
                                .error
                                .unwrap_or_else(|| "Unknown local agent error".to_string());
                            warn!(
                                agent = %self.target_agent_id,
                                request_id = %request_id,
                                error = %error,
                                "Local exec failed"
                            );
                            // Include stdout/stderr so the agent can see what
                            // went wrong, not just the error summary.
                            let stdout = result.stdout.unwrap_or_default();
                            let stderr = result.stderr.unwrap_or_default();
                            let detail = if stdout.is_empty() && stderr.is_empty() {
                                error
                            } else {
                                let mut parts = vec![error];
                                if !stdout.is_empty() {
                                    parts.push(format!("\n--- stdout ---\n{stdout}"));
                                }
                                if !stderr.is_empty() {
                                    parts.push(format!("\n--- stderr ---\n{stderr}"));
                                }
                                parts.join("")
                            };
                            Ok(ToolResult::failure(detail, elapsed))
                        }
                    }
                    Ok(Err(_)) => {
                        // Agent disconnected — oneshot sender was dropped
                        warn!(
                            agent = %self.target_agent_id,
                            "Local agent disconnected during exec — falling back to gateway"
                        );
                        self.fallback_exec.execute(input).await
                    }
                    Err(_) => {
                        // Timeout
                        let elapsed = start.elapsed().as_millis() as u64;
                        warn!(
                            agent = %self.target_agent_id,
                            request_id = %request_id,
                            timeout_secs = timeout.as_secs(),
                            "Local exec timed out"
                        );
                        Ok(ToolResult::failure(
                            format!(
                                "Local agent did not respond within {} seconds.",
                                timeout.as_secs()
                            ),
                            elapsed,
                        ))
                    }
                }
            }
            Err(e) => {
                // Agent not connected — fall back to gateway
                warn!(
                    agent = %self.target_agent_id,
                    error = %e,
                    "Local agent not available — falling back to gateway"
                );
                self.fallback_exec.execute(input).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protected_paths() {
        assert!(is_protected_path("cat SOUL.md"));
        assert!(!is_protected_path("echo hello > MEMORY.md"));
        assert!(is_protected_path("ls /workspace/memory/daily.md"));
        assert!(is_protected_path("cat /workspace/notes/2024-01-01.md"));
        assert!(is_protected_path("rm -rf /.local/state"));
        assert!(is_protected_path("cat IDENTITY.md"));
        assert!(is_protected_path("cat HEARTBEAT.md"));
        assert!(is_protected_path("cat WELCOME.md"));
        assert!(is_protected_path("cat AGENTS.md"));
        assert!(is_protected_path("cat TOOLS.md"));

        // Regular commands should not be protected
        assert!(!is_protected_path("ls -la"));
        assert!(!is_protected_path("git status"));
        assert!(!is_protected_path("npm install"));
        assert!(!is_protected_path("cargo build"));
    }
}

//! Tools that run on a person's own machine over the node uplink.
//!
//! [`LocalAgentExecTool`] stands in for `exec.run` when a session selected a
//! local agent; [`NodeTool`] is one tool of an MCP server a node tunnels,
//! offered to its person's sessions as `node.<node id>.<tool>`. Each call
//! checks that the turn runs as the node's person: a session naming a node
//! is never enough to reach it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use tracing::{info, warn};

use workforce_agent::tools::{Tool, ToolResult};
use workforce_gateway_client::protocol::{LocalExecRequest, LocalExecResult};

use crate::local_agent_registry::{node_of, outcome, LocalAgentRegistry, NodeCall, OfferedTool};

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

/// How long a node tool call may take before it is given up on.
const NODE_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

/// Check whether a command references a protected path.
fn is_protected_path(command: &str) -> bool {
    PROTECTED_PATTERNS.iter().any(|p| command.contains(p))
}

/// The person this turn runs as, whom a node must act for.
fn caller() -> Option<String> {
    crate::user_config::current_identity_id()
}

/// Tool implementation that routes `exec.run` to a connected local agent.
/// Commands touching protected identity files run on the gateway's own exec
/// tool; a node that is gone or acts for someone else fails the call rather
/// than running it on the hub instead.
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

        let elapsed = || start.elapsed().as_millis() as u64;
        let Some(caller) = caller() else {
            return Ok(ToolResult::failure(
                "A local agent runs commands only for its own person's turns.",
                elapsed(),
            ));
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let request = LocalExecRequest {
            msg_type: "local_exec".to_string(),
            request_id: request_id.clone(),
            command: exec_input.command.clone(),
            workdir: exec_input.workdir.clone(),
            timeout_secs: exec_input.timeout_secs,
        };
        let call = NodeCall::Exec(request);
        let mut events = match self
            .local_agents
            .call(&self.target_agent_id, &caller, call)
            .await
        {
            Ok(events) => events,
            Err(e) => {
                warn!(agent = %self.target_agent_id, error = %e, "Local exec not sent");
                return Ok(ToolResult::failure(
                    format!("Not run: {e}. Nothing ran anywhere."),
                    elapsed(),
                ));
            }
        };
        let timeout = Duration::from_secs(exec_input.timeout_secs.unwrap_or(300));
        let ran = tokio::time::timeout(timeout, outcome(&mut events))
            .await
            .map(|reply| reply.map(exec_result));
        match ran {
            Ok(Some(Ok(result))) if result.ok => {
                info!(
                    agent = %self.target_agent_id,
                    request_id = %request_id,
                    duration_ms = elapsed(),
                    "Local exec succeeded"
                );
                let output = serde_json::json!({
                    "stdout": result.stdout.unwrap_or_default(),
                    "stderr": result.stderr.unwrap_or_default(),
                    "exit_code": result.exit_code.unwrap_or(0),
                    "source": "local_agent",
                    "agent_id": self.target_agent_id,
                });
                Ok(ToolResult::success(output, elapsed()))
            }
            Ok(Some(Ok(result))) => {
                let error = result
                    .error
                    .unwrap_or_else(|| "Unknown local agent error".to_string());
                warn!(
                    agent = %self.target_agent_id,
                    request_id = %request_id,
                    error = %error,
                    "Local exec failed"
                );
                // Include stdout/stderr so the agent can see what went
                // wrong, not just the error summary.
                let stdout = result.stdout.unwrap_or_default();
                let stderr = result.stderr.unwrap_or_default();
                let mut detail = error;
                if !stdout.is_empty() {
                    detail.push_str(&format!("\n--- stdout ---\n{stdout}"));
                }
                if !stderr.is_empty() {
                    detail.push_str(&format!("\n--- stderr ---\n{stderr}"));
                }
                Ok(ToolResult::failure(detail, elapsed()))
            }
            Ok(Some(Err(error))) => {
                warn!(agent = %self.target_agent_id, request_id = %request_id, %error, "Local exec failed");
                Ok(ToolResult::failure(error, elapsed()))
            }
            Ok(None) => {
                warn!(agent = %self.target_agent_id, "Local agent disconnected during exec");
                Ok(ToolResult::failure(
                    "The local agent disconnected before the command finished; it may have run \
                     in part.",
                    elapsed(),
                ))
            }
            Err(_) => {
                warn!(
                    agent = %self.target_agent_id,
                    request_id = %request_id,
                    timeout_secs = timeout.as_secs(),
                    "Local exec timed out"
                );
                self.local_agents
                    .cancel(&self.target_agent_id, &request_id)
                    .await;
                Ok(ToolResult::failure(
                    format!(
                        "Local agent did not respond within {} seconds.",
                        timeout.as_secs()
                    ),
                    elapsed(),
                ))
            }
        }
    }
}

/// An exec call's outcome: the command's result, or why there is none.
fn exec_result(
    reply: workforce_gateway_client::protocol::NodeToolResult,
) -> Result<LocalExecResult, String> {
    match (reply.result, reply.error) {
        (_, Some(error)) => Err(error),
        (Some(result), None) => serde_json::from_value(result)
            .map_err(|e| format!("The local agent's result was unreadable: {e}")),
        (None, None) => Err("The local agent answered without a result".to_string()),
    }
}

/// One tool a node offers its person's sessions, run on that node.
pub struct NodeTool {
    local_agents: Arc<LocalAgentRegistry>,
    offered: OfferedTool,
    description: String,
}

impl NodeTool {
    pub fn new(local_agents: Arc<LocalAgentRegistry>, offered: OfferedTool) -> Self {
        let description = format!(
            "On {} (the user's own machine): {}",
            offered.node_name, offered.spec.description
        );
        Self {
            local_agents,
            offered,
            description,
        }
    }
}

#[async_trait]
impl Tool for NodeTool {
    fn name(&self) -> &str {
        &self.offered.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.offered.spec.input_schema.clone()
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let name = &self.offered.name;
        let Some(caller) = caller() else {
            return Ok(ToolResult::failure(
                format!("{name} runs only for its own person's turns."),
                elapsed(),
            ));
        };
        let Some(node) = node_of(name) else {
            return Ok(ToolResult::failure(
                format!("{name} is not a node tool"),
                elapsed(),
            ));
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let call = NodeCall::tool(request_id.clone(), self.offered.spec.name.clone(), input);
        let mut events = match self.local_agents.call(node, &caller, call).await {
            Ok(events) => events,
            Err(e) => return Ok(ToolResult::failure(format!("Not run: {e}."), elapsed())),
        };
        match tokio::time::timeout(NODE_TOOL_TIMEOUT, outcome(&mut events)).await {
            Ok(Some(reply)) => Ok(match (reply.result, reply.error) {
                (Some(result), None) if !is_error(&result) => {
                    ToolResult::success(result, elapsed())
                }
                (Some(result), None) => ToolResult::failure(error_text(&result), elapsed()),
                (_, Some(error)) => ToolResult::failure(error, elapsed()),
                (None, None) => ToolResult::failure(
                    format!("{} answered without a result", self.offered.node_name),
                    elapsed(),
                ),
            }),
            Ok(None) => Ok(ToolResult::failure(
                format!(
                    "{} disconnected before {name} finished; it may have run in part.",
                    self.offered.node_name
                ),
                elapsed(),
            )),
            Err(_) => {
                self.local_agents.cancel(node, &request_id).await;
                Ok(ToolResult::failure(
                    format!(
                        "{} did not answer {name} within {} seconds.",
                        self.offered.node_name,
                        NODE_TOOL_TIMEOUT.as_secs()
                    ),
                    elapsed(),
                ))
            }
        }
    }
}

/// Whether an MCP `tools/call` result reports the tool failed.
fn is_error(result: &serde_json::Value) -> bool {
    result.get("isError").and_then(|v| v.as_bool()) == Some(true)
}

/// A failed MCP `tools/call` result's text items, or the result itself
/// when it carries none.
fn error_text(result: &serde_json::Value) -> String {
    let text: Vec<&str> = result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["text"].as_str())
        .collect();
    match text.is_empty() {
        true => result.to_string(),
        false => text.join("\n"),
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

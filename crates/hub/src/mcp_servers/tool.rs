//! [`Tool`] implementation that forwards `tools/call` to an MCP
//! server. One `McpTool` per (server, upstream tool) — the
//! `mcp_servers::server` module constructs one on registration.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{info_span, warn, Instrument};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::cost::CostTracker;
use workforce_mcp_client::{McpClient, McpError, McpToolDef};

/// One agent-callable tool backed by an upstream MCP server.
///
/// `name` is the agent-facing form (`mcp.<server_id>.<tool>`),
/// `upstream_name` is the bare tool name the upstream server
/// expects in `tools/call`. They differ because we namespace
/// every MCP tool to avoid collisions across servers + with the
/// agent's built-in tools.
pub struct McpTool {
    name: String,
    upstream_name: String,
    description: String,
    input_schema: Value,
    server_id: String,
    client: Arc<McpClient>,
    cost: Option<Arc<CostTracker>>,
    agent_id: String,
}

impl McpTool {
    pub fn new(
        server_id: impl Into<String>,
        agent_id: impl Into<String>,
        client: Arc<McpClient>,
        def: McpToolDef,
        cost: Option<Arc<CostTracker>>,
    ) -> Self {
        let server_id = server_id.into();
        let name = format!("mcp.{server_id}.{}", def.name);
        Self {
            name,
            upstream_name: def.name,
            description: def.description,
            input_schema: def.input_schema,
            server_id,
            client,
            cost,
            agent_id: agent_id.into(),
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        // Call-time gate for MCP workspaces: a dormant member (a gated env not
        // active for this session) is not callable, even if a stale tool name
        // survived in the conversation history. The tool filter already hides
        // dormant tools from the catalogue; this is the defence-in-depth that
        // makes the safety property hold regardless of how the name got here.
        if !workforce_agent::llm::mcp_workspace::is_server_active(&self.server_id) {
            let msg = workforce_agent::llm::mcp_workspace::dormant_call_message(&self.server_id);
            return Ok(ToolResult::failure(msg, 0));
        }
        let span = info_span!(
            "mcp.tool_call",
            vendor = %self.server_id,
            tool = %self.upstream_name,
            outcome = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        );
        let start = Instant::now();
        let cancel = CancellationToken::new();
        let result = self
            .client
            .tools_call(&self.upstream_name, input, cancel)
            .instrument(span.clone())
            .await;
        let duration_ms = start.elapsed().as_millis() as u64;
        let outcome = match &result {
            Ok(_) => "ok",
            Err(McpError::Cancelled) => "cancelled",
            Err(McpError::AuthExpired(_)) => "auth_expired",
            Err(McpError::RateLimited { .. }) => "rate_limited",
            Err(McpError::BodyTooLarge { .. }) => "body_too_large",
            Err(McpError::SchemaViolation(_)) => "schema_violation",
            Err(McpError::ToolCountExceeded { .. }) => "tool_count_exceeded",
            Err(McpError::RawToolLimitExceeded { .. }) => "raw_tool_limit_exceeded",
            Err(McpError::ServerError { .. }) => "server_error",
            Err(McpError::Unreachable(_)) => "unreachable",
            Err(McpError::Transport(_)) => "transport_error",
            Err(McpError::Protocol(_)) => "protocol_error",
        };
        span.record("outcome", outcome);
        span.record("duration_ms", duration_ms);
        metrics::counter!(
            "mcp.calls",
            "vendor" => self.server_id.clone(),
            "tool" => self.upstream_name.clone(),
            "outcome" => outcome,
        )
        .increment(1);
        metrics::histogram!(
            "mcp.duration_ms",
            "vendor" => self.server_id.clone(),
            "tool" => self.upstream_name.clone(),
        )
        .record(duration_ms as f64);
        if let Some(ref tracker) = self.cost {
            let description = format!("mcp:{}:{}", self.server_id, self.upstream_name);
            tracker.record(workforce_common::cost::CostEntry {
                description: Some(&description),
                ..workforce_common::cost::CostEntry::new(
                    &self.agent_id,
                    "mcp",
                    workforce_common::cost::TokenCounts::default(),
                    workforce_common::subscription::Origin::McpTool,
                )
            });
        }

        if matches!(result, Err(McpError::AuthExpired(_))) {
            warn!(server = %self.server_id, "MCP auth expired — operator should re-authorize");
        }
        Ok(map_call_result(result, duration_ms))
    }
}

fn map_call_result(result: Result<Value, McpError>, duration_ms: u64) -> ToolResult {
    match result {
        Ok(value) => ToolResult::success(value, duration_ms),
        Err(McpError::RateLimited {
            message,
            retry_after_secs,
        }) => ToolResult::rate_limited(
            format!("rate limited: {message}"),
            retry_after_secs,
            duration_ms,
        ),
        Err(e) => ToolResult::failure(e.to_string(), duration_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_limited_mcp_call_carries_the_typed_signal() {
        let result = map_call_result(
            Err(McpError::RateLimited {
                message: "HTTP 429 from https://mcp.example/: slow down".into(),
                retry_after_secs: Some(30),
            }),
            5,
        );
        assert!(!result.success);
        assert_eq!(
            result.rate_limited.and_then(|l| l.retry_after_secs),
            Some(30)
        );
        assert!(result.error.unwrap().contains("rate limited: HTTP 429"));
    }

    #[test]
    fn other_mcp_errors_stay_untyped() {
        let result = map_call_result(Err(McpError::Transport("boom".into())), 5);
        assert!(!result.success);
        assert!(result.rate_limited.is_none());
    }
}

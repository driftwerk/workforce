//! Dynamic MCP registration for every tool exposed by the Jira / Linear /
//! Datadog adapter skills.
//!
//! Each `workforce_agent::Tool` already carries its own name, description,
//! and JSON Schema. Instead of writing one `#[tool]`-decorated wrapper per
//! underlying tool, we iterate every loaded adapter at startup and feed the
//! metadata straight into rmcp's `ToolRouter`. A single shared handler
//! forwards the call to the right adapter based on the tool's namespace
//! prefix (`jira.*`, `linear.*`, `datadog.*`).

use std::sync::Arc;

use futures::future::BoxFuture;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolName;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{JsonObject, Tool as RmcpTool};
use workforce_agent::llm::tool_schema::sanitize_tool_schema;
use workforce_agent::tools::Tool;

use crate::adapters::{AdapterHandle, Adapters};
use crate::LocalMcpServer;

/// Build a router populated with one rmcp route per underlying skill tool
/// for every adapter that successfully loaded credentials. Adapters without
/// credentials contribute nothing, so `tools/list` only advertises tools
/// that can actually be called.
pub(crate) fn build_adapter_router(adapters: &Adapters) -> ToolRouter<LocalMcpServer> {
    [
        adapters.jira.as_ref(),
        adapters.linear.as_ref(),
        adapters.datadog.as_ref(),
        adapters.metabase.as_ref(),
        adapters.postmark.as_ref(),
        adapters.sendgrid.as_ref(),
        adapters.xero.as_ref(),
    ]
    .into_iter()
    .flatten()
    .flat_map(AdapterHandle::tools)
    .map(rmcp_tool_for)
    .fold(ToolRouter::new(), |router, rmcp_tool| {
        router.with_route((rmcp_tool, passthrough))
    })
}

fn rmcp_tool_for(tool: &Arc<dyn Tool>) -> RmcpTool {
    // Fold top-level `oneOf` / `allOf` / `anyOf` — Claude Code → Anthropic 400s otherwise.
    let schema = match sanitize_tool_schema(tool.input_schema()) {
        serde_json::Value::Object(map) => map,
        _ => JsonObject::new(),
    };
    RmcpTool::new(
        tool.name().to_string(),
        tool.description().to_string(),
        Arc::new(schema),
    )
}

/// Shared handler for every dynamically-registered adapter tool. Returns an
/// explicit `BoxFuture` (matching rmcp's `AsyncMethodAdapter` signature
/// expectation) so we can register it as a `fn` pointer in the router
/// without going through `#[tool]` per call.
///
/// Dispatches on the tool name's namespace prefix to find the right
/// [`AdapterHandle`], then forwards the JSON args through.
fn passthrough<'s>(
    server: &'s LocalMcpServer,
    ToolName(name): ToolName,
    Parameters(args): Parameters<JsonObject>,
) -> BoxFuture<'s, Result<String, String>> {
    Box::pin(async move {
        let adapter = match name.split_once('.').map(|(ns, _)| ns) {
            Some("jira") => server.state.adapters.jira.as_ref(),
            Some("linear") => server.state.adapters.linear.as_ref(),
            Some("datadog") => server.state.adapters.datadog.as_ref(),
            Some("metabase") => server.state.adapters.metabase.as_ref(),
            Some("postmark") => server.state.adapters.postmark.as_ref(),
            Some("sendgrid") => server.state.adapters.sendgrid.as_ref(),
            Some("xero") => server.state.adapters.xero.as_ref(),
            _ => None,
        };
        let handle = adapter.ok_or_else(|| {
            format!(
                "adapter for '{name}' is not loaded; run `workforce-local-mcp config <adapter>`"
            )
        })?;
        let output = handle.call(&name, serde_json::Value::Object(args)).await?;
        Ok(output.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use eyre::Result;
    use workforce_agent::tools::ToolResult;

    /// Root-level `oneOf` (same shape as `google.drive.upload`). Guards
    /// the local-mcp → Claude Code → Anthropic chain: `rmcp_tool_for`
    /// must strip it before the router hands the schema to Claude Code.
    struct OneOfTool;

    #[async_trait]
    impl Tool for OneOfTool {
        fn name(&self) -> &str {
            "test.one_of"
        }
        fn description(&self) -> &str {
            "Tool with root-level oneOf — Anthropic rejects this shape"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "a": { "type": "string" },
                    "b": { "type": "string" }
                },
                "oneOf": [{ "required": ["a"] }, { "required": ["b"] }]
            })
        }
        async fn execute(&self, _input: serde_json::Value) -> Result<ToolResult> {
            Ok(ToolResult::success(serde_json::Value::Null, 1))
        }
    }

    #[test]
    fn rmcp_tool_for_strips_top_level_constraint_keys() {
        let tool: Arc<dyn Tool> = Arc::new(OneOfTool);
        let rmcp_tool = rmcp_tool_for(&tool);
        let schema = serde_json::to_value(&rmcp_tool.input_schema).unwrap();
        assert!(
            schema.get("oneOf").is_none(),
            "router schema must not expose top-level oneOf — Anthropic rejects it"
        );
        assert!(schema.get("allOf").is_none());
        assert!(schema.get("anyOf").is_none());
        assert_eq!(schema["type"], serde_json::json!("object"));
    }
}

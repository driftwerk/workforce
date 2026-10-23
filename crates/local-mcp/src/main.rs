//! Local Claude Code MCP server — exposes workforce's codegraph and memory
//! capabilities to a locally-running Claude Code session over stdio.
//!
//! Designed for single-user developer use on macOS. Reuses the workforce
//! crates directly (SQLite code graph, SQLite FTS5 memory) without any
//! cluster dependencies (no Weaviate, no Postgres, no NATS).

use std::sync::Arc;

use clap::Parser;
use eyre::Result;
use rmcp::{
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Content, ErrorData, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
    RoleServer, ServerHandler, ServiceExt,
};

mod adapters;
mod cli;
mod config;
mod embedding;
mod hubs;
mod mirror;
mod paths;
mod remote;
mod secret_store;
mod state;
mod tools;

use state::AppState;

#[derive(Clone)]
pub(crate) struct LocalMcpServer {
    state: Arc<AppState>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PingArgs {
    /// Optional echo string — returned verbatim in the reply.
    #[serde(default)]
    message: Option<String>,
}

#[tool_router(router = core_router)]
impl LocalMcpServer {
    fn new(state: Arc<AppState>) -> Self {
        let adapter_router = tools::adapters::build_adapter_router(&state.adapters);
        Self {
            state,
            tool_router: Self::core_router()
                + Self::codegraph_router()
                + Self::memory_router()
                + Self::project_router()
                + Self::impact_router()
                + Self::fuse_router()
                + Self::remote_router()
                + adapter_router,
        }
    }

    #[tool(
        description = "Liveness probe. Returns the server name, version, and echoes the optional \
                       `message` argument. Useful for verifying the MCP connection."
    )]
    async fn ping(&self, Parameters(args): Parameters<PingArgs>) -> String {
        serde_json::json!({
            "server": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
            "echo": args.message.unwrap_or_default(),
            "data_dir": self.state.data_dir.display().to_string(),
            "workspace": self.state.workspace.display().to_string(),
        })
        .to_string()
    }
}

/// How long the first `tools/list` waits for the startup hub connection so
/// a healthy hub's tools are in the initial list without `initialize` ever
/// blocking on it.
const REMOTE_READY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Static tools first, then the active hub's `wf.*` tools, sorted by name.
/// A static route wins a name collision so `call_tool` and `list_tools` agree.
fn merge_tool_lists(static_tools: Vec<Tool>, remote_tools: Vec<Tool>) -> Vec<Tool> {
    let taken: std::collections::HashSet<_> = static_tools.iter().map(|t| t.name.clone()).collect();
    let mut tools: Vec<Tool> = static_tools
        .into_iter()
        .chain(
            remote_tools
                .into_iter()
                .filter(|t| !taken.contains(&t.name)),
        )
        .collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LocalMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_instructions(
            "workforce-local-mcp: codegraph + memory tools for Claude Code. \
             Run `codegraph.index_repo` once per repo, then use `codegraph.find_symbol` / \
             `codegraph.callers` / `codegraph.callees` / `codegraph.file_graph` to explore. \
             Use `memory.remember` / `memory.recall` to persist and retrieve cross-session notes. \
             `wf.*` tools act on the active remote workforce hub as you (`wf.hubs` lists hubs, \
             `wf.use` switches).",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.state.remote.wait_ready(REMOTE_READY_WAIT).await;
        Ok(ListToolsResult {
            tools: merge_tool_lists(self.tool_router.list_all(), self.state.remote.tools()),
            meta: None,
            next_cursor: None,
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router
            .get(name)
            .cloned()
            .or_else(|| self.state.remote.tool(name))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if self.tool_router.has_route(&request.name) {
            return self
                .tool_router
                .call(ToolCallContext::new(self, request, context))
                .await;
        }
        if !request.name.starts_with(remote::REMOTE_PREFIX) {
            return Err(ErrorData::invalid_params("tool not found", None));
        }
        let arguments = request.arguments.unwrap_or_default();
        Ok(
            match self
                .state
                .remote
                .call(&request.name, arguments, context.ct)
                .await
            {
                Ok(text) => CallToolResult::success(vec![Content::text(text)]),
                Err(error) => CallToolResult::error(vec![Content::text(error)]),
            },
        )
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if cli::Cli::parse().dispatch()? {
        return Ok(());
    }

    // Logs go to stderr so stdout stays clean for the MCP transport.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cfg = config::LocalMcpConfig::load()?;
    tracing::info!(
        data_dir = %cfg.data_dir.display(),
        workspace = %cfg.workspace.display(),
        "local-mcp starting",
    );

    let state = Arc::new(AppState::new(cfg).await?);
    // Kick off the auto-pull / re-index scheduler. Drops on process exit;
    // we leak the join handle deliberately because the server runs forever.
    let _mirror_task = state.mirror.spawn();
    let server = LocalMcpServer::new(state);

    let service = server
        .serve(stdio())
        .await
        .map_err(|e| eyre::eyre!("failed to start stdio MCP server: {e}"))?;
    service
        .waiting()
        .await
        .map_err(|e| eyre::eyre!("MCP server terminated with error: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Tool {
        Tool::new(
            name.to_string(),
            name.to_string(),
            Arc::new(Default::default()),
        )
    }

    #[test]
    fn merged_list_keeps_static_tools_and_adds_remote_ones_sorted() {
        let merged = merge_tool_lists(
            vec![tool("ping"), tool("wf.hubs"), tool("codegraph.callers")],
            vec![tool("wf.workflow"), tool("wf.hubs"), tool("wf.agent_chat")],
        );
        let names: Vec<_> = merged.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(
            names,
            vec![
                "codegraph.callers",
                "ping",
                "wf.agent_chat",
                "wf.hubs",
                "wf.workflow"
            ]
        );
    }
}

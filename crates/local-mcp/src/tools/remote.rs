//! `wf.hubs` / `wf.use` — inspect and switch the active remote workforce
//! hub. The hub's own tools are advertised as `wf.<name>` by
//! [`crate::LocalMcpServer`]'s `list_tools` and forwarded by `call_tool`.

use rmcp::{
    handler::server::wrapper::Parameters, schemars, service::RoleServer, tool, tool_router, Peer,
};
use serde::Deserialize;

use crate::remote::ActiveState;
use crate::LocalMcpServer;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UseArgs {
    /// Name of a hub registered with `workforce-local-mcp config workforce add`.
    pub name: String,
}

#[tool_router(router = remote_router, vis = "pub(crate)")]
impl LocalMcpServer {
    #[tool(
        name = "wf.hubs",
        description = "List the registered remote workforce hubs, which one is active, and \
                       whether it is connected (with its remote tool count). Register hubs with \
                       `workforce-local-mcp config workforce add <name> --url <url>`."
    )]
    async fn wf_hubs(&self) -> String {
        serde_json::to_string_pretty(&self.state.remote.status()).expect("status serializes")
    }

    #[tool(
        name = "wf.use",
        description = "Switch the active remote workforce hub. Reconnects, replaces the advertised \
                       `wf.*` tools with the new hub's, and persists the choice for future sessions."
    )]
    async fn wf_use(
        &self,
        peer: Peer<RoleServer>,
        Parameters(args): Parameters<UseArgs>,
    ) -> Result<String, String> {
        let state = self
            .state
            .remote
            .switch(&args.name)
            .await
            .map_err(|e| e.to_string())?;
        if let Err(e) = peer.notify_tool_list_changed().await {
            tracing::warn!(error = %e, "could not notify the client that the tool list changed");
        }
        match state {
            ActiveState::Connected { tool_count } => Ok(format!(
                "Active hub is now {:?} with {tool_count} remote tools available as wf.*",
                args.name
            )),
            ActiveState::Disconnected { error } => Err(format!(
                "Active hub is now {:?} but it is unreachable ({error}); no wf.* tools are available until it responds",
                args.name
            )),
        }
    }
}

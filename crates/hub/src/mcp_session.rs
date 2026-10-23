//! A per-session MCP server the hub owns.
//!
//! Under privilege separation Claude Code runs as the unprivileged user, so a
//! server it spawned itself would inherit that uid and could not decrypt the
//! vault its credentialed tools need. The hub therefore serves the session from
//! its own registry — which is also the only registry holding `wasm_skill.*`
//! and the installed skills' own tools.
//!
//! The listener is tied to the [`McpSession`] value: dropping it aborts the
//! task, so a turn that ends by any route cannot leave one serving.

use eyre::{eyre, Result};
use tracing::{debug, warn};

use crate::mcp::McpEndpoint;

/// A listening MCP server owned by one Claude Code turn.
pub struct McpSession {
    task: tokio::task::JoinHandle<()>,
    port: u16,
    token: String,
}

impl McpSession {
    /// Serve `endpoint` on a loopback port for the lifetime of this value.
    pub async fn start(endpoint: McpEndpoint) -> Result<Self> {
        let token = uuid::Uuid::new_v4().to_string();
        let token_for_task = token.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| eyre!("cannot bind an MCP listener: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| eyre!("cannot read the MCP listener address: {e}"))?
            .port();

        // A serve error can only surface after the caller has the port, so it is
        // logged rather than returned; the turn then fails on its first tool call.
        let task = tokio::spawn(async move {
            if let Err(e) = crate::mcp::serve_http(endpoint, listener, token_for_task).await {
                warn!(error = %e, "in-process MCP server stopped");
            }
        });
        debug!(port, "MCP session serving");
        Ok(Self { task, port, token })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Where a client on this host reaches the server.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    use workforce_agent::tools::registry::ToolRegistry;
    use workforce_agent::tools::{Tool, ToolResult};

    /// Reports the identity its call resolved under, which is the only way to
    /// see a scope that is otherwise invisible until a vault lookup goes wrong.
    struct WhoAmI;

    #[async_trait::async_trait]
    impl Tool for WhoAmI {
        fn name(&self) -> &str {
            "test.whoami"
        }
        fn description(&self) -> &str {
            "identity the call resolved under"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
            let who = workforce_agent::runner::CURRENT_USER_ID
                .try_with(|id| id.clone())
                .unwrap_or_else(|_| "<unscoped>".to_string());
            Ok(ToolResult::success(serde_json::json!({"identity": who}), 0))
        }
    }

    async fn call_whoami(port: u16, token: &str) -> String {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "test_whoami", "arguments": {}}
        });
        let resp: serde_json::Value = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/mcp"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
        serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|v| v["identity"].as_str().map(String::from))
            .unwrap_or_else(|| text.to_string())
    }

    fn whoami(identity: Option<&str>) -> McpEndpoint {
        named_whoami("workforce-test", None, identity)
    }

    fn named_whoami(
        server_name: &str,
        instructions: Option<&str>,
        identity: Option<&str>,
    ) -> McpEndpoint {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(WhoAmI));
        let executor: Arc<dyn crate::mcp::ToolCallExecutor> = match identity {
            Some(identity) => Arc::new(As(identity.to_string())),
            None => Arc::new(crate::mcp::DirectExecutor),
        };
        McpEndpoint::new(
            crate::mcp::Surface::Fixed(Arc::new(registry)),
            server_name.into(),
            instructions.map(str::to_string),
            executor,
        )
    }

    /// Runs every call as one identity, as a round's executor does.
    struct As(String);

    #[async_trait::async_trait]
    impl crate::mcp::ToolCallExecutor for As {
        async fn call(
            &self,
            tool: Arc<dyn Tool>,
            _name: &str,
            input: serde_json::Value,
        ) -> eyre::Result<crate::mcp::McpReply> {
            workforce_agent::runner::CURRENT_USER_ID
                .scope(self.0.clone(), tool.execute(input))
                .await
                .map(crate::mcp::McpReply::from_result)
        }
    }

    /// The behaviour change that would fail silently: one hub serves many
    /// sessions, so the identity must belong to the request. If it leaked
    /// between them, a lookup would resolve another user's vault paths.
    #[tokio::test]
    async fn concurrent_sessions_each_resolve_their_own_identity() {
        let one = McpSession::start(whoami(Some("uid-one"))).await.unwrap();
        let two = McpSession::start(whoami(Some("uid-two"))).await.unwrap();

        let (a, b) = tokio::join!(
            call_whoami(one.port(), one.token()),
            call_whoami(two.port(), two.token())
        );
        assert_eq!(a, "uid-one");
        assert_eq!(b, "uid-two");

        // And again, interleaved, so a scope set by one cannot linger for the other.
        let (b2, a2) = tokio::join!(
            call_whoami(two.port(), two.token()),
            call_whoami(one.port(), one.token())
        );
        assert_eq!(b2, "uid-two");
        assert_eq!(a2, "uid-one");
    }

    /// The session MCP is what every hub-spawned Claude Code turn talks to.
    /// This pins the full exchange — initialize, tools/list, tools/call — so a
    /// change to the shared dispatch that alters what a session sees fails here.
    #[tokio::test]
    async fn a_session_serves_the_full_exchange_unchanged() {
        let session = McpSession::start(named_whoami(
            "workforce-allan",
            Some("served for a turn"),
            Some("uid-turn"),
        ))
        .await
        .unwrap();
        let client = reqwest::Client::new();
        let post = |body: serde_json::Value| {
            client
                .post(format!("http://127.0.0.1:{}/mcp", session.port()))
                .header("Authorization", format!("Bearer {}", session.token()))
                .json(&body)
                .send()
        };

        let init: serde_json::Value = post(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "x"}}
        }))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "workforce-allan");
        assert_eq!(init["result"]["instructions"], "served for a turn");
        assert_eq!(
            init["result"]["capabilities"],
            serde_json::json!({"tools": {}})
        );

        let initialized = post(serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }))
        .await
        .unwrap();
        assert_eq!(initialized.status(), 202);

        let listed: serde_json::Value = post(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list"
        }))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "test_whoami");
        assert_eq!(
            tools[0]["inputSchema"],
            serde_json::json!({"type": "object"})
        );

        assert_eq!(
            call_whoami(session.port(), session.token()).await,
            "uid-turn"
        );
    }

    #[tokio::test]
    async fn a_session_without_an_identity_scopes_nothing() {
        let s = McpSession::start(whoami(None)).await.unwrap();
        assert_eq!(call_whoami(s.port(), s.token()).await, "<unscoped>");
    }

    #[tokio::test]
    async fn one_sessions_token_is_refused_by_another() {
        let one = McpSession::start(whoami(None)).await.unwrap();
        let two = McpSession::start(whoami(None)).await.unwrap();
        let refused = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/mcp", two.port()))
            .header("Authorization", format!("Bearer {}", one.token()))
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), 401);
    }

    /// Dropping the session must stop the listener, as dropping it killed the
    /// process — otherwise a turn's server outlives the turn.
    #[tokio::test]
    async fn dropping_a_session_stops_serving() {
        let port = {
            let s = McpSession::start(whoami(None)).await.unwrap();
            let p = s.port();
            assert_eq!(call_whoami(p, s.token()).await, "<unscoped>");
            p
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/mcp"))
                .body("{}")
                .send()
                .await
                .is_err(),
            "the listener must be gone once the session is dropped"
        );
    }
}

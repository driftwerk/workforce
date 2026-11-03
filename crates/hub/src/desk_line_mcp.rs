//! Desk's line over MCP, for the engines that take tools that way (Codex
//! now): the hub's own [`McpEndpoint`] over the line's tools, each bound to
//! the session whose hook token the request came with. The tools are
//! `workforce_desk::line::TOOLS`, and every call goes to the one dispatch
//! Desk's Claude Code plugin uses too, [`Desk::line_call`].

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_desk::line::{LineTool, TOOLS};
use workforce_desk::{Desk, SessionMcp};

use crate::mcp::{DirectExecutor, McpEndpoint, Surface};

/// What a session's engine is told of the line when it connects.
const INSTRUCTIONS: &str = "Desk's line: ask or tell Desk (the person's overseer), the person, or another session. Routine questions are answered by Desk; anything that pushes, deletes, deploys, spends or changes scope goes to the person.";

/// One line tool, bound to the session calling it.
struct LineToolHandle {
    caller: Uuid,
    def: &'static LineTool,
    desk: Desk,
}

#[async_trait]
impl Tool for LineToolHandle {
    fn name(&self) -> &str {
        self.def.name
    }

    fn description(&self) -> &str {
        self.def.description
    }

    fn input_schema(&self) -> serde_json::Value {
        (self.def.schema)()
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let started = std::time::Instant::now();
        let ms = || started.elapsed().as_millis() as u64;
        Ok(
            match self
                .desk
                .line_call(self.caller, self.def.name, &input, None)
                .await
            {
                Ok(result) => ToolResult::success(result, ms()),
                Err(said) => ToolResult::failure(said, ms()),
            },
        )
    }
}

/// Answers sessions' MCP requests on their lines.
pub(crate) struct LineMcp {
    desk: Desk,
}

impl LineMcp {
    pub(crate) fn new(desk: Desk) -> Arc<Self> {
        Arc::new(Self { desk })
    }

    fn endpoint(&self, caller: Uuid) -> McpEndpoint {
        let registry = ToolRegistry::new();
        for def in &TOOLS {
            registry.register(Arc::new(LineToolHandle {
                caller,
                def,
                desk: self.desk.clone(),
            }));
        }
        McpEndpoint::new(
            Surface::Fixed(Arc::new(registry)),
            "desk".into(),
            Some(INSTRUCTIONS.into()),
            Arc::new(DirectExecutor),
        )
    }
}

#[async_trait]
impl SessionMcp for LineMcp {
    async fn answer(&self, caller: Uuid, body: &str) -> Option<String> {
        self.endpoint(caller).handle(body).await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use workforce_postgres::test_support::with_test_pool;

    use super::*;
    use crate::desk_crew::tests::{desk, hook_url, new_session};

    async fn rpc(url: &str, body: Value) -> (reqwest::StatusCode, Value) {
        let response = reqwest::Client::new()
            .post(url)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn the_line_over_mcp_lists_its_tools_and_calls_them_as_the_tokens_session() {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let s = new_session(&d, dir.path(), "acme-web").await;
            let hook = hook_url(dir.path(), s.id).await;
            let url = hook.replace("/hook/", "/mcp/");
            // Not served until the hub installs it.
            let (status, _) = rpc(&url, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).await;
            assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
            d.serve_lines(LineMcp::new(d.clone()));

            let (_, listed) = rpc(&url, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).await;
            let names: Vec<&str> = listed["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap())
                .collect();
            let mut want: Vec<&str> = TOOLS.iter().map(|t| t.name).collect();
            let mut got = names.clone();
            want.sort();
            got.sort();
            assert_eq!(got, want);

            let (_, told) = rpc(
                &url,
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": {"name": "tell", "arguments": {"to": "you", "text": "PR is up"}}}),
            )
            .await;
            assert_ne!(told["result"]["isError"], true, "{told}");
            let thread = d.line_thread(s.id, None, None).await.unwrap();
            assert_eq!(thread.len(), 1);
            assert_eq!(thread[0].from.id, s.id.to_string(), "as the token's session");
            assert_eq!(thread[0].text, "PR is up");

            let (_, refused) = rpc(
                &url,
                json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": {"name": "reply", "arguments": {"message_id": Uuid::new_v4(), "text": "x"}}}),
            )
            .await;
            assert_eq!(refused["result"]["isError"], true, "{refused}");

            // A wrong token is answered nothing.
            let wrong = format!("{}nope", &url[..url.rfind('/').unwrap() + 1]);
            let (status, body) = rpc(&wrong, json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"})).await;
            assert_eq!((status, body), (reqwest::StatusCode::NOT_FOUND, Value::Null));
        })
        .await;
    }
}

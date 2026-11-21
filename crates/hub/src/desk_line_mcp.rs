//! Desk's line over MCP, for the engines that take tools that way (Codex
//! now): the hub's own [`McpEndpoint`] over the line's tools, each bound to
//! the session whose hook token the request came with. The tools are
//! `workforce_desk::line::TOOLS`, and every call goes to the one dispatch
//! Desk's Claude Code plugin uses too, [`Desk::line_call`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use uuid::Uuid;
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_desk::line::LineTool;
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

/// How long a session's tool list serves its calls, a `tools/list` always
/// reading it afresh.
const LISTED_FOR: Duration = Duration::from_secs(5);

type Listed = HashMap<Uuid, (Instant, Vec<&'static LineTool>)>;

/// Answers sessions' MCP requests on their lines.
pub(crate) struct LineMcp {
    desk: Desk,
    /// Each session's tools as last worked out, so a `tools/call` doesn't
    /// read the board again.
    listed: parking_lot::Mutex<Listed>,
}

impl LineMcp {
    pub(crate) fn new(desk: Desk) -> Arc<Self> {
        Arc::new(Self {
            desk,
            listed: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    /// The tools offered to `caller`: as worked out in the last few
    /// seconds unless `fresh`, which a `tools/list` is.
    async fn tools(&self, caller: Uuid, fresh: bool) -> Vec<&'static LineTool> {
        if !fresh {
            if let Some((at, tools)) = self.listed.lock().get(&caller) {
                if at.elapsed() < LISTED_FOR {
                    return tools.clone();
                }
            }
        }
        let tools = self.desk.line_tools(caller).await;
        let mut listed = self.listed.lock();
        listed.retain(|_, (at, _)| at.elapsed() < LISTED_FOR);
        listed.insert(caller, (Instant::now(), tools.clone()));
        tools
    }

    async fn endpoint(&self, caller: Uuid, fresh: bool) -> McpEndpoint {
        let registry = ToolRegistry::new();
        for def in self.tools(caller, fresh).await {
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
        let fresh = serde_json::from_str::<serde_json::Value>(body)
            .map_or(true, |v| v["method"] == "tools/list");
        self.endpoint(caller, fresh).await.handle(body).await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use workforce_postgres::test_support::with_test_pool;

    use workforce_desk::line::TOOLS;

    use super::*;
    use crate::desk_crew::tests::{desk, hook_url, new_session};

    /// Linear reached, Jira not.
    struct OnlyLinear;

    #[async_trait::async_trait]
    impl workforce_desk::issues::Issues for OnlyLinear {
        async fn issue(
            &self,
            _: &workforce_desk::issues::Asker,
            _: &workforce_desk::issues::Asked,
        ) -> Result<Value, String> {
            Err("not here".into())
        }

        async fn offered(
            &self,
            _: &workforce_desk::issues::Asker,
        ) -> Vec<workforce_desk::board::System> {
            vec![workforce_desk::board::System::Linear]
        }
    }

    /// Linear reached, counting the times it is asked.
    struct Counting(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl workforce_desk::issues::Issues for Counting {
        async fn issue(
            &self,
            _: &workforce_desk::issues::Asker,
            _: &workforce_desk::issues::Asked,
        ) -> Result<Value, String> {
            Err("not here".into())
        }

        async fn offered(
            &self,
            _: &workforce_desk::issues::Asker,
        ) -> Vec<workforce_desk::board::System> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            vec![workforce_desk::board::System::Linear]
        }
    }

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

            let listed = async || {
                let (_, listed) =
                    rpc(&url, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).await;
                let mut names: Vec<String> = listed["result"]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t["name"].as_str().unwrap().to_string())
                    .collect();
                names.sort();
                names
            };
            let want = |offered: &[&str]| {
                let mut want: Vec<String> = TOOLS
                    .iter()
                    .map(|t| t.name)
                    .filter(|n| {
                        workforce_desk::issues::system_of_tool(n).is_none() || offered.contains(n)
                    })
                    .map(str::to_string)
                    .collect();
                want.sort();
                want
            };
            // An issue tool is offered only where its system is reached.
            assert_eq!(listed().await, want(&[]));
            d.issues_with(Arc::new(OnlyLinear));
            assert_eq!(listed().await, want(&["linear"]));

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

            // Codex's app-server sends the token as a bearer token.
            let (bare, token) = url.rsplit_once('/').unwrap();
            let bearer = |t: &str| {
                reqwest::Client::new()
                    .post(bare)
                    .bearer_auth(t)
                    .json(&json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list"}))
                    .send()
            };
            let ok = bearer(token).await.unwrap();
            assert_eq!(ok.status(), reqwest::StatusCode::OK);
            let listed: Value = ok.json().await.unwrap();
            assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), TOOLS.len() - 1);
            assert_eq!(bearer("nope").await.unwrap().status(), reqwest::StatusCode::NOT_FOUND);

            // A wrong token is answered nothing.
            let wrong = format!("{}nope", &url[..url.rfind('/').unwrap() + 1]);
            let (status, body) = rpc(&wrong, json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"})).await;
            assert_eq!((status, body), (reqwest::StatusCode::NOT_FOUND, Value::Null));
        })
        .await;
    }

    /// A call doesn't read the board again for the tools; a list does.
    #[tokio::test]
    async fn calls_reuse_the_tools_a_list_worked_out() {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let s = new_session(&d, dir.path(), "acme-web").await;
            let url = hook_url(dir.path(), s.id).await.replace("/hook/", "/mcp/");
            let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            d.issues_with(Arc::new(Counting(Arc::clone(&reads))));
            d.serve_lines(LineMcp::new(d.clone()));
            let count = || reads.load(std::sync::atomic::Ordering::SeqCst);

            rpc(
                &url,
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            )
            .await;
            assert_eq!(count(), 1);
            for id in 2..5 {
                let (_, told) = rpc(
                    &url,
                    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                        "params": {"name": "tell", "arguments": {"to": "you", "text": "hi"}}}),
                )
                .await;
                assert_ne!(told["result"]["isError"], true, "{told}");
            }
            assert_eq!(count(), 1, "calls read the board again");
            rpc(
                &url,
                json!({"jsonrpc": "2.0", "id": 9, "method": "tools/list"}),
            )
            .await;
            assert_eq!(count(), 2, "a list reads it afresh");
        })
        .await;
    }
}

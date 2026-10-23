//! End-to-end test for the `[mcp.servers.*]` startup pipeline.
//! Stands up a wiremock-faked MCP server, calls
//! `mcp_servers::spawn_all`, and verifies tools are registered
//! and callable through the agent's `ToolRegistry`.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_common::settings::{McpAuthConfig, McpServerConfig, WorkforceConfig};
use workforce_hub::mcp_servers::spawn_all;
use workforce_hub::vault::Vault;

fn json_response(id: u64, result: Value) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/json")
        .insert_header("mcp-session-id", "sess-1")
        .set_body_json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn rpc_id(req: &Request) -> u64 {
    serde_json::from_slice::<Value>(&req.body).unwrap()["id"]
        .as_u64()
        .unwrap_or(0)
}

fn rpc_method(req: &Request) -> String {
    serde_json::from_slice::<Value>(&req.body).unwrap()["method"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

struct BodyRpc(&'static str);
impl wiremock::Match for BodyRpc {
    fn matches(&self, req: &Request) -> bool {
        rpc_method(req) == self.0
    }
}

fn config_for(server_uri: &str) -> WorkforceConfig {
    let mut config = WorkforceConfig::default();
    let mut servers = HashMap::new();
    servers.insert(
        "test".to_string(),
        McpServerConfig {
            endpoint: server_uri.to_string(),
            transport: "streamable-http".to_string(),
            auth: McpAuthConfig::Pat {
                vault_path: "skills/test/token".to_string(),
            },
            allowed_hosts: Vec::new(),
            refresh_interval_secs: 3600,
            shadows: Vec::new(),
            fallback_to_builtin: false,
            tools_allow: Vec::new(),
            tools_deny: Vec::new(),
            workspace: None,
            env: None,
            default: false,
        },
    );
    config.mcp.servers = servers;
    config
}

async fn mount_initialize(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/"))
        .and(BodyRpc("initialize"))
        .respond_with(|req: &Request| {
            json_response(
                rpc_id(req),
                json!({
                    "protocolVersion": "2025-06-18",
                    "serverInfo": {"name": "test-server", "version": "0.1.0"},
                    "capabilities": {"tools": {}}
                }),
            )
        })
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(server)
        .await;
}

fn tmp_vault() -> (TempDir, Arc<Vault>) {
    let tmp = TempDir::new().unwrap();
    let vault_path = tmp.path().join("vault.json");
    let vault = Vault::open(&vault_path, "test-master-key").unwrap();
    vault.set("skills/test/token", "ghp_pat_value").unwrap();
    (tmp, Arc::new(vault))
}

#[tokio::test]
async fn spawn_all_registers_upstream_tools_under_namespaced_prefix() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(|req: &Request| {
            json_response(
                rpc_id(req),
                json!({
                    "tools": [
                        {"name": "ping", "description": "round-trip", "inputSchema": {"type": "object"}},
                        {"name": "echo", "description": "mirror", "inputSchema": {"type": "object"}},
                    ]
                }),
            )
        })
        .mount(&server)
        .await;

    let config = config_for(&server.uri());
    let (_tmp, vault) = tmp_vault();
    let tools = Arc::new(ToolRegistry::new());

    let registry = spawn_all(
        &config,
        "test-agent",
        Arc::clone(&tools),
        Some(Arc::clone(&vault)),
        Some(Arc::clone(&vault)),
        None,
    )
    .await;

    let names: Vec<String> = tools.names();
    assert!(
        names.iter().any(|n| n == "mcp.test.ping"),
        "tools registered = {names:?}"
    );
    assert!(names.iter().any(|n| n == "mcp.test.echo"));

    let summaries = registry.summaries();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, "test");
    assert_eq!(summaries[0].tool_count, 2);
    assert_eq!(summaries[0].auth_kind, "pat");
}

#[tokio::test]
async fn spawn_all_skips_server_when_initialize_fails() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(BodyRpc("initialize"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream broken"))
        .mount(&server)
        .await;

    let config = config_for(&server.uri());
    let (_tmp, vault) = tmp_vault();
    let tools = Arc::new(ToolRegistry::new());

    // Should NOT panic; the server is logged and skipped.
    let registry = spawn_all(
        &config,
        "test-agent",
        Arc::clone(&tools),
        Some(Arc::clone(&vault)),
        Some(Arc::clone(&vault)),
        None,
    )
    .await;

    assert!(tools.names().is_empty(), "no tools registered on failure");
    assert!(
        registry.summaries().is_empty(),
        "registry has no entries when init fails"
    );
}

#[tokio::test]
async fn registered_mcp_tool_forwards_call_to_upstream() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(|req: &Request| {
            json_response(
                rpc_id(req),
                json!({
                    "tools": [
                        {"name": "echo", "description": "mirror", "inputSchema": {"type": "object"}},
                    ]
                }),
            )
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(BodyRpc("tools/call"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["params"]["name"], "echo");
            assert_eq!(body["params"]["arguments"]["msg"], "hi");
            json_response(rpc_id(req), json!({"got": "hi"}))
        })
        .mount(&server)
        .await;

    let config = config_for(&server.uri());
    let (_tmp, vault) = tmp_vault();
    let tools = Arc::new(ToolRegistry::new());

    spawn_all(
        &config,
        "test-agent",
        Arc::clone(&tools),
        Some(Arc::clone(&vault)),
        Some(Arc::clone(&vault)),
        None,
    )
    .await;

    let tool = tools
        .get("mcp.test.echo")
        .expect("mcp.test.echo registered");
    let result = tool.execute(json!({"msg": "hi"})).await.unwrap();
    assert!(result.success, "tool call should succeed: {result:?}");
    assert_eq!(result.output["got"], "hi");
}

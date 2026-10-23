//! End-to-end test for [`McpToolAccess`] — install an MCP server
//! with a shadow plan against a wiremock upstream, then verify that
//! the per-user visibility decision flips correctly based on
//! `fallback_to_builtin` and the user's token state.

use std::sync::Arc;

use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use workforce_agent::tools::access::{ToolAccess, ToolAccessSnapshot};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_common::settings::{McpAuthBinding, McpAuthConfig, McpServerConfig};
use workforce_hub::mcp_servers::{McpServerManager, McpToolAccess};
use workforce_hub::vault::Vault;
use workforce_mcp_store::{InstalledVia, McpServerStore};
use workforce_oauth_client::InMemoryOAuthStateStore;

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

async fn mount_pat_mcp(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/"))
        .and(BodyRpc("initialize"))
        .respond_with(|req: &Request| {
            json_response(
                rpc_id(req),
                json!({
                    "protocolVersion": "2025-06-18",
                    "serverInfo": {"name": "linear", "version": "0.1.0"},
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
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(|req: &Request| {
            json_response(
                rpc_id(req),
                json!({
                    "tools": [
                        {"name": "create_issue", "description": "", "inputSchema": {"type": "object"}},
                        {"name": "list_issues", "description": "", "inputSchema": {"type": "object"}},
                    ]
                }),
            )
        })
        .mount(server)
        .await;
}

struct Fixture {
    _tmp: TempDir,
    vault: Arc<Vault>,
    tools: Arc<ToolRegistry>,
    manager: Arc<McpServerManager>,
}

impl Fixture {
    fn access(&self) -> McpToolAccess {
        McpToolAccess::new(
            self.manager.registry(),
            Arc::clone(&self.tools),
            Some(Arc::clone(&self.vault)),
            Some(Arc::clone(&self.vault)),
        )
    }
}

fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let vault_path = tmp.path().join("vault.json");
    let vault = Arc::new(Vault::open(&vault_path, "test-key").unwrap());
    let tools = Arc::new(ToolRegistry::new());
    let store: Arc<dyn McpServerStore> =
        Arc::new(workforce_mcp_store::sqlite::SqliteMcpServerStore::in_memory().unwrap());
    let oauth_state: Arc<dyn workforce_oauth_client::OAuthStateStore> =
        Arc::new(InMemoryOAuthStateStore::default());
    let manager = McpServerManager::new(
        "test-agent",
        store,
        Arc::clone(&tools),
        Some(Arc::clone(&vault)),
        Some(Arc::clone(&vault)),
        oauth_state,
        None,
        Arc::new(reqwest::Client::new()),
    );
    Fixture {
        _tmp: tmp,
        vault,
        tools,
        manager,
    }
}

fn pat_cfg(uri: &str, fallback: bool) -> McpServerConfig {
    McpServerConfig {
        endpoint: uri.to_string(),
        transport: "streamable-http".to_string(),
        auth: McpAuthConfig::Pat {
            vault_path: "skills/linear/api_key".to_string(),
        },
        allowed_hosts: Vec::new(),
        refresh_interval_secs: 3600,
        shadows: vec!["linear_*".to_string()],
        fallback_to_builtin: fallback,
        tools_allow: Vec::new(),
        tools_deny: Vec::new(),
        workspace: None,
        env: None,
        default: false,
    }
}

fn oauth_cfg(uri: &str, fallback: bool) -> McpServerConfig {
    McpServerConfig {
        endpoint: uri.to_string(),
        transport: "streamable-http".to_string(),
        auth: McpAuthConfig::Oauth2 {
            binding: McpAuthBinding::User,
            provider: "linear".to_string(),
            scopes: Vec::new(),
            shared_pat_fallback: None,
            discovery: None,
        },
        allowed_hosts: Vec::new(),
        refresh_interval_secs: 3600,
        shadows: vec!["linear_*".to_string()],
        fallback_to_builtin: fallback,
        tools_allow: Vec::new(),
        tools_deny: Vec::new(),
        workspace: None,
        env: None,
        default: false,
    }
}

fn add_builtin_linear_tools(tools: &ToolRegistry) {
    use async_trait::async_trait;
    use workforce_agent::tools::{Tool, ToolResult};
    struct Noop(&'static str);
    #[async_trait]
    impl Tool for Noop {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "builtin"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(&self, _: Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult::success(json!(null), 0))
        }
    }
    // Real adapters register DOTTED internal names; the underscored
    // `linear_*` shadow patterns must still match these via
    // `shadows::matches` normalisation. (Previously these fixtures used
    // underscored names, which masked the routing bug.)
    tools.register(Arc::new(Noop("linear.issue_create")));
    tools.register(Arc::new(Noop("linear.issue_get")));
}

#[tokio::test]
async fn pat_with_token_hides_builtins() {
    let upstream = MockServer::start().await;
    mount_pat_mcp(&upstream).await;
    let f = fixture();
    add_builtin_linear_tools(&f.tools);
    f.vault.set("skills/linear/api_key", "lin_pat").unwrap();

    f.manager
        .install("linear", pat_cfg(&upstream.uri(), false), InstalledVia::Api)
        .await
        .unwrap();

    let snap = f.access().snapshot();
    assert!(snap.is_blocked("linear.issue_create"));
    assert!(snap.is_blocked("linear.issue_get"));
    assert!(!snap.is_blocked("mcp.linear.create_issue"));
}

#[tokio::test]
async fn pat_missing_with_fallback_hides_mcp() {
    let upstream = MockServer::start().await;
    mount_pat_mcp(&upstream).await;
    let f = fixture();
    add_builtin_linear_tools(&f.tools);
    f.vault.set("skills/linear/api_key", "lin_pat").unwrap();
    f.manager
        .install("linear", pat_cfg(&upstream.uri(), true), InstalledVia::Api)
        .await
        .unwrap();
    f.vault.delete("skills/linear/api_key").unwrap();

    let snap = f.access().snapshot();
    assert!(snap.is_blocked("mcp.linear.create_issue"));
    assert!(snap.is_blocked("mcp.linear.list_issues"));
    assert!(!snap.is_blocked("linear.issue_create"));
}

/// Drive an OAuth install for `binding=User` by scoping the install
/// future under `installer_uid`, with a synthetic access_token seeded
/// at that user's vault path so the upstream `initialize()` skips the
/// refresh flow entirely. Returns the live manager + vault.
async fn install_oauth_user_bound(
    f: &Fixture,
    upstream_uri: &str,
    fallback: bool,
    installer_uid: &str,
) {
    f.vault.set("mcp/v1/linear/client_id", "x").unwrap();
    f.vault.set("mcp/v1/linear/client_secret", "y").unwrap();
    f.vault
        .set(
            "mcp/v1/linear/redirect_uri",
            "https://hub.example/api/auth/linear/callback",
        )
        .unwrap();
    let base = format!("users/{installer_uid}/mcp/v1/linear");
    f.vault
        .set(&format!("{base}/access_token"), "fresh")
        .unwrap();
    f.vault
        .set(&format!("{base}/expires_at"), "9999999999")
        .unwrap();
    f.vault.set(&format!("{base}/refresh_token"), "rt").unwrap();
    let cfg = oauth_cfg(upstream_uri, fallback);
    let manager = Arc::clone(&f.manager);
    let uid = installer_uid.to_string();
    workforce_agent::runner::CURRENT_USER_ID
        .scope(uid, async move {
            manager
                .install("linear", cfg, InstalledVia::Agent)
                .await
                .expect("OAuth install should succeed under CURRENT_USER_ID")
        })
        .await;
}

async fn snapshot_as_user(access: &McpToolAccess, uid: &str) -> ToolAccessSnapshot {
    workforce_agent::runner::CURRENT_USER_ID
        .scope(uid.to_string(), async { access.snapshot() })
        .await
}

#[tokio::test]
async fn oauth_user_without_token_no_fallback_hides_builtins_with_authorise_hint() {
    let upstream = MockServer::start().await;
    mount_pat_mcp(&upstream).await;
    let f = fixture();
    add_builtin_linear_tools(&f.tools);
    install_oauth_user_bound(&f, &upstream.uri(), false, "u1").await;
    let snap = snapshot_as_user(&f.access(), "u2").await;
    let reason = snap.block_reason("linear.issue_create").expect("blocked");
    assert!(
        reason.contains("authoris") || reason.contains("authorize"),
        "expected authorise hint, got: {reason}"
    );
    assert!(!snap.is_blocked("mcp.linear.create_issue"));
}

#[tokio::test]
async fn oauth_user_with_fallback_no_token_hides_mcp() {
    let upstream = MockServer::start().await;
    mount_pat_mcp(&upstream).await;
    let f = fixture();
    add_builtin_linear_tools(&f.tools);
    install_oauth_user_bound(&f, &upstream.uri(), true, "u1").await;
    let snap = snapshot_as_user(&f.access(), "u2").await;
    assert!(snap.is_blocked("mcp.linear.create_issue"));
    assert!(snap.is_blocked("mcp.linear.list_issues"));
    assert!(!snap.is_blocked("linear.issue_create"));
}

#[tokio::test]
async fn oauth_user_with_token_hides_builtins() {
    let upstream = MockServer::start().await;
    mount_pat_mcp(&upstream).await;
    let f = fixture();
    add_builtin_linear_tools(&f.tools);
    install_oauth_user_bound(&f, &upstream.uri(), true, "u1").await;
    let snap = snapshot_as_user(&f.access(), "u1").await;
    assert!(snap.is_blocked("linear.issue_create"));
    assert!(!snap.is_blocked("mcp.linear.create_issue"));
}

#[tokio::test]
async fn empty_plans_yield_empty_snapshot() {
    let f = fixture();
    assert!(f.access().snapshot().is_empty());
}

//! End-to-end tests for `McpServerManager` — the runtime owner of
//! agent-driven MCP server installs (PR5).
//!
//! Each test stands up:
//! - a wiremock-faked MCP server (initialise + tools/list)
//! - a SQLite-backed `McpServerStore` (in-memory)
//! - an in-memory `OAuthStateStore`
//! - a fresh `Vault` for the agent
//!
//! …and exercises one piece of the lifecycle: install → tools live,
//! uninstall → tools gone, hydrate-on-boot, TOML import, etc.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use workforce_agent::tools::registry::ToolRegistry;
use workforce_common::settings::{McpAuthConfig, McpConfig, McpServerConfig};
use workforce_hub::hosts::McpHost;
use workforce_hub::mcp_servers::McpServerManager;
use workforce_hub::vault::Vault;
use workforce_mcp_store::{InstalledVia, McpServerStore};
use workforce_oauth_client::{InMemoryOAuthStateStore, OAuthStateStore};

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

async fn mount_initialize_and_tools(server: &MockServer, tools: Value) {
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
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(move |req: &Request| json_response(rpc_id(req), tools.clone()))
        .mount(server)
        .await;
}

fn pat_config(server_uri: &str) -> McpServerConfig {
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
    }
}

fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let vault_path = tmp.path().join("vault.json");
    let vault = Vault::open(&vault_path, "test-master-key").unwrap();
    vault.set("skills/test/token", "ghp_pat_value").unwrap();
    let vault = Arc::new(vault);
    Fixture {
        _tmp: tmp,
        store: Arc::new(workforce_mcp_store::sqlite::SqliteMcpServerStore::in_memory().unwrap()),
        oauth_state: Arc::new(InMemoryOAuthStateStore::new()),
        tools: Arc::new(ToolRegistry::new()),
        vault,
    }
}

struct Fixture {
    _tmp: TempDir,
    store: Arc<dyn McpServerStore>,
    oauth_state: Arc<dyn OAuthStateStore>,
    tools: Arc<ToolRegistry>,
    vault: Arc<Vault>,
}

impl Fixture {
    fn manager(&self, agent_id: &str) -> Arc<McpServerManager> {
        McpServerManager::new(
            agent_id.to_string(),
            Arc::clone(&self.store),
            Arc::clone(&self.tools),
            Some(Arc::clone(&self.vault)),
            Some(Arc::clone(&self.vault)),
            Arc::clone(&self.oauth_state),
            None,
            Arc::new(reqwest::Client::new()),
        )
    }
}

#[tokio::test]
async fn install_registers_tools_and_persists_to_store() {
    let server = MockServer::start().await;
    mount_initialize_and_tools(
        &server,
        json!({
            "tools": [
                {"name": "ping", "description": "round-trip", "inputSchema": {"type": "object"}},
                {"name": "echo", "description": "mirror", "inputSchema": {"type": "object"}},
            ]
        }),
    )
    .await;

    let f = fixture();
    let manager = f.manager("test-agent");
    let report = manager
        .install("test", pat_config(&server.uri()), InstalledVia::Agent)
        .await
        .unwrap();
    let report_value = serde_json::to_value(&report).unwrap();
    assert_eq!(report_value["status"], "installed");
    assert_eq!(report_value["tool_count"], 2);

    let names = f.tools.names();
    assert!(names.iter().any(|n| n == "mcp.test.ping"));
    assert!(names.iter().any(|n| n == "mcp.test.echo"));

    // Persisted to store with provenance == Agent.
    let row = f.store.get("test-agent", "test").unwrap().unwrap();
    assert_eq!(row.installed_via, InstalledVia::Agent);
}

fn three_tool_server_body() -> Value {
    json!({
        "tools": [
            {"name": "issue_create", "description": "", "inputSchema": {"type": "object"}},
            {"name": "issue_delete", "description": "", "inputSchema": {"type": "object"}},
            {"name": "comment_list", "description": "", "inputSchema": {"type": "object"}},
        ]
    })
}

#[tokio::test]
async fn allowlist_filters_on_install_and_survives_refresh() {
    let server = MockServer::start().await;
    mount_initialize_and_tools(&server, three_tool_server_body()).await;

    let f = fixture();
    let manager = f.manager("test-agent");
    // Allow `issue_*` but deny `issue_delete`: only `issue_create` survives.
    let mut cfg = pat_config(&server.uri());
    cfg.tools_allow = vec!["issue_*".to_string()];
    cfg.tools_deny = vec!["issue_delete".to_string()];

    let report = manager
        .install("test", cfg, InstalledVia::Agent)
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(&report).unwrap()["tool_count"], 1);
    assert!(f.tools.get("mcp.test.issue_create").is_some());
    assert!(f.tools.get("mcp.test.issue_delete").is_none());
    assert!(f.tools.get("mcp.test.comment_list").is_none());

    // The on-demand refresh path (`mcp.update`) re-fetches the full
    // upstream list; the allowlist must hold so denied tools don't slip
    // back in.
    let refreshed = manager.refresh("test").await.unwrap();
    assert_eq!(refreshed.tool_count, 1);
    assert!(f.tools.get("mcp.test.issue_create").is_some());
    assert!(f.tools.get("mcp.test.issue_delete").is_none());
    assert!(f.tools.get("mcp.test.comment_list").is_none());
}

#[tokio::test]
async fn set_tool_filter_hot_swaps_curated_surface() {
    let server = MockServer::start().await;
    mount_initialize_and_tools(&server, three_tool_server_body()).await;

    let f = fixture();
    let manager = f.manager("test-agent");
    // Install with no filter — the agent first sees the full surface.
    manager
        .install("test", pat_config(&server.uri()), InstalledVia::Agent)
        .await
        .unwrap();
    assert!(f.tools.get("mcp.test.comment_list").is_some());

    // Curate to `issue_*` minus `issue_delete`, applied hot.
    let report = manager
        .set_tool_filter(
            "test",
            vec!["issue_*".to_string()],
            vec!["issue_delete".to_string()],
        )
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(&report).unwrap()["tool_count"], 1);
    assert!(f.tools.get("mcp.test.issue_create").is_some());
    assert!(f.tools.get("mcp.test.issue_delete").is_none());
    assert!(f.tools.get("mcp.test.comment_list").is_none());

    // The curation is persisted on the stored config for next boot.
    let row = f.store.get("test-agent", "test").unwrap().unwrap();
    let cfg: McpServerConfig = serde_json::from_value(row.config).unwrap();
    assert_eq!(cfg.tools_allow, vec!["issue_*".to_string()]);
    assert_eq!(cfg.tools_deny, vec!["issue_delete".to_string()]);
}

#[tokio::test]
async fn set_tool_filter_rejects_unknown_server() {
    let f = fixture();
    let manager = f.manager("test-agent");
    let err = manager
        .set_tool_filter("never-installed", vec!["*".to_string()], vec![])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not installed"), "got {err}");
}

#[tokio::test]
async fn uninstall_unregisters_tools_and_drops_store_row() {
    let server = MockServer::start().await;
    mount_initialize_and_tools(
        &server,
        json!({
            "tools": [
                {"name": "echo", "description": "mirror", "inputSchema": {"type": "object"}},
            ]
        }),
    )
    .await;
    let f = fixture();
    let manager = f.manager("test-agent");
    manager
        .install("test", pat_config(&server.uri()), InstalledVia::Agent)
        .await
        .unwrap();
    assert!(f.tools.get("mcp.test.echo").is_some());
    assert!(f.store.get("test-agent", "test").unwrap().is_some());

    let report = manager.uninstall("test", false).await.unwrap();
    assert_eq!(report.removed_tools, vec!["mcp.test.echo".to_string()]);
    assert!(!report.purged_tokens, "no tokens to purge for PAT-auth");

    assert!(f.tools.get("mcp.test.echo").is_none());
    assert!(f.store.get("test-agent", "test").unwrap().is_none());
}

#[tokio::test]
async fn uninstall_succeeds_for_never_installed_server() {
    let f = fixture();
    let manager = f.manager("test-agent");
    let report = manager.uninstall("never-was-here", false).await.unwrap();
    assert!(report.removed_tools.is_empty());
    assert!(!report.purged_tokens);
}

#[tokio::test]
async fn hydrate_skips_malformed_store_rows() {
    let f = fixture();
    // Pre-seed a row whose `config` JSON is missing required fields.
    let bogus = workforce_mcp_store::McpServerRecord::new(
        "test-agent",
        "borked",
        json!({"not": "an mcp server config"}),
        InstalledVia::Agent,
    );
    f.store.put(&bogus).unwrap();

    let manager = f.manager("test-agent");
    // hydrate must not error — it logs and skips bad rows so one
    // bad install doesn't keep the whole hub from booting.
    manager.hydrate_from_store().await.unwrap();
    assert!(manager.list().is_empty());
}

#[tokio::test]
async fn list_reflects_currently_installed_servers() {
    let server = MockServer::start().await;
    mount_initialize_and_tools(
        &server,
        json!({
            "tools": [{"name": "echo", "description": "mirror", "inputSchema": {"type": "object"}}]
        }),
    )
    .await;
    let f = fixture();
    let manager = f.manager("test-agent");
    assert!(manager.list().is_empty());
    manager
        .install("alpha", pat_config(&server.uri()), InstalledVia::Agent)
        .await
        .unwrap();
    let summaries = manager.list();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, "alpha");
    assert_eq!(summaries[0].auth_kind, "pat");
}

#[tokio::test]
async fn show_returns_config_and_summary_when_installed() {
    let server = MockServer::start().await;
    mount_initialize_and_tools(
        &server,
        json!({"tools": [{"name": "echo", "description": "", "inputSchema": {"type": "object"}}]}),
    )
    .await;
    let f = fixture();
    let manager = f.manager("test-agent");
    manager
        .install("alpha", pat_config(&server.uri()), InstalledVia::Agent)
        .await
        .unwrap();
    let detail = manager.show("alpha").unwrap().unwrap();
    assert_eq!(detail.summary.id, "alpha");
    assert_eq!(detail.installed_via, "agent");
    assert_eq!(detail.config["transport"], "streamable-http");
}

#[tokio::test]
async fn show_returns_none_when_not_installed() {
    let f = fixture();
    let manager = f.manager("test-agent");
    assert!(manager.show("nothing").unwrap().is_none());
}

#[tokio::test]
async fn refresh_picks_up_newly_added_tools() {
    let server = MockServer::start().await;
    let calls = Arc::new(parking_lot::Mutex::new(0u32));
    let calls_clone = Arc::clone(&calls);
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
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(move |req: &Request| {
            let mut n = calls_clone.lock();
            *n += 1;
            let tools = if *n == 1 {
                json!({"tools": [{"name": "first", "description": "", "inputSchema": {"type": "object"}}]})
            } else {
                json!({
                    "tools": [
                        {"name": "first", "description": "", "inputSchema": {"type": "object"}},
                        {"name": "second", "description": "", "inputSchema": {"type": "object"}},
                    ]
                })
            };
            json_response(rpc_id(req), tools)
        })
        .mount(&server)
        .await;

    let f = fixture();
    let manager = f.manager("test-agent");
    manager
        .install("alpha", pat_config(&server.uri()), InstalledVia::Agent)
        .await
        .unwrap();
    assert!(f.tools.get("mcp.alpha.first").is_some());
    assert!(f.tools.get("mcp.alpha.second").is_none());

    let report = manager.refresh("alpha").await.unwrap();
    assert_eq!(report.tool_count, 2);
    assert!(f.tools.get("mcp.alpha.second").is_some());
}

#[tokio::test]
async fn refresh_unknown_server_returns_error() {
    let f = fixture();
    let manager = f.manager("test-agent");
    let err = manager.refresh("nothing").await.unwrap_err();
    assert!(err.to_string().contains("is not installed"), "{err}");
}

#[tokio::test]
async fn install_collision_replaces_previous() {
    let server = MockServer::start().await;
    mount_initialize_and_tools(
        &server,
        json!({
            "tools": [
                {"name": "v1", "description": "", "inputSchema": {"type": "object"}}
            ]
        }),
    )
    .await;

    let f = fixture();
    let manager = f.manager("test-agent");
    manager
        .install("alpha", pat_config(&server.uri()), InstalledVia::Agent)
        .await
        .unwrap();
    assert!(f.tools.get("mcp.alpha.v1").is_some());

    // Re-install with same id (same upstream just with new auth path).
    let mut cfg = pat_config(&server.uri());
    if let McpAuthConfig::Pat { vault_path } = &mut cfg.auth {
        *vault_path = "skills/test/token".to_string();
    }
    manager
        .install("alpha", cfg, InstalledVia::Agent)
        .await
        .unwrap();
    // v1 is still registered (no change in upstream tools), but the
    // server entry was replaced cleanly without orphaning the
    // previous task.
    assert!(f.tools.get("mcp.alpha.v1").is_some());
    assert_eq!(manager.list().len(), 1);
}

#[tokio::test]
async fn hydrate_from_store_respawns_persisted_servers() {
    let server = MockServer::start().await;
    mount_initialize_and_tools(
        &server,
        json!({
            "tools": [{"name": "echo", "description": "", "inputSchema": {"type": "object"}}]
        }),
    )
    .await;

    let f = fixture();
    // Pre-seed store as if a previous hub run had installed it.
    let cfg = pat_config(&server.uri());
    let cfg_value = serde_json::to_value(&cfg).unwrap();
    let record = workforce_mcp_store::McpServerRecord::new(
        "test-agent",
        "alpha",
        cfg_value,
        InstalledVia::Agent,
    );
    f.store.put(&record).unwrap();

    // Brand-new manager (simulates a hub restart).
    let manager = f.manager("test-agent");
    assert!(f.tools.get("mcp.alpha.echo").is_none());
    manager.hydrate_from_store().await.unwrap();
    assert!(f.tools.get("mcp.alpha.echo").is_some());
    assert_eq!(manager.list().len(), 1);
}

#[tokio::test]
async fn hydrate_registers_binding_user_server_via_enrolled_token() {
    let server = MockServer::start().await;
    mount_initialize_and_tools(
        &server,
        json!({"tools": [{"name": "echo", "description": "", "inputSchema": {"type": "object"}}]}),
    )
    .await;

    let f = fixture();
    // A previous run's OAuth client + one enrolled user's token. Hydrate runs
    // with no ambient user, so registering `echo` proves the discovery
    // identity borrowed the enrolled user to authenticate `tools/list`.
    f.vault.set("mcp/v1/ux/client_id", "cid").unwrap();
    f.vault
        .set("users/kings/mcp/v1/ux/access_token", "at-stored")
        .unwrap();
    f.vault
        .set("users/kings/mcp/v1/ux/expires_at", "9999999999")
        .unwrap();

    let cfg = McpServerConfig {
        endpoint: server.uri(),
        transport: "streamable-http".to_string(),
        auth: McpAuthConfig::Oauth2 {
            binding: workforce_common::settings::McpAuthBinding::User,
            provider: "linear".to_string(),
            scopes: Vec::new(),
            shared_pat_fallback: None,
            discovery: None,
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
    };
    let record = workforce_mcp_store::McpServerRecord::new(
        "test-agent",
        "ux",
        serde_json::to_value(&cfg).unwrap(),
        InstalledVia::Agent,
    );
    f.store.put(&record).unwrap();

    let manager = f.manager("test-agent");
    manager.hydrate_from_store().await.unwrap();

    assert!(f.tools.get("mcp.ux.echo").is_some());
    assert_eq!(manager.list().len(), 1);
}

#[tokio::test]
async fn import_from_toml_seeds_new_entries_only() {
    let server = MockServer::start().await;
    mount_initialize_and_tools(
        &server,
        json!({
            "tools": [{"name": "echo", "description": "", "inputSchema": {"type": "object"}}]
        }),
    )
    .await;

    let f = fixture();
    let manager = f.manager("test-agent");

    let mut servers = HashMap::new();
    servers.insert("from-toml".to_string(), pat_config(&server.uri()));
    let toml_cfg = McpConfig {
        servers,
        ..McpConfig::default()
    };
    manager.import_from_toml(&toml_cfg).await.unwrap();

    let row = f.store.get("test-agent", "from-toml").unwrap().unwrap();
    assert_eq!(row.installed_via, InstalledVia::Toml);
    assert!(f.tools.get("mcp.from-toml.echo").is_some());

    // Second import should be a no-op (does not overwrite the existing row).
    let before = f.store.get("test-agent", "from-toml").unwrap().unwrap();
    manager.import_from_toml(&toml_cfg).await.unwrap();
    let after = f.store.get("test-agent", "from-toml").unwrap().unwrap();
    assert_eq!(before.created_at, after.created_at);
}

#[tokio::test]
async fn import_from_toml_does_not_clobber_agent_install() {
    let server = MockServer::start().await;
    mount_initialize_and_tools(
        &server,
        json!({"tools": [{"name": "echo", "description": "", "inputSchema": {"type": "object"}}]}),
    )
    .await;
    let f = fixture();
    let manager = f.manager("test-agent");
    manager
        .install("alpha", pat_config(&server.uri()), InstalledVia::Agent)
        .await
        .unwrap();
    let agent_row = f.store.get("test-agent", "alpha").unwrap().unwrap();
    assert_eq!(agent_row.installed_via, InstalledVia::Agent);

    let mut servers = HashMap::new();
    servers.insert("alpha".to_string(), pat_config(&server.uri()));
    let toml_cfg = McpConfig {
        servers,
        ..McpConfig::default()
    };
    manager.import_from_toml(&toml_cfg).await.unwrap();
    let after = f.store.get("test-agent", "alpha").unwrap().unwrap();
    assert_eq!(
        after.installed_via,
        InstalledVia::Agent,
        "TOML import must not stomp agent-installed provenance"
    );
}

#[tokio::test]
async fn install_with_failing_upstream_returns_error_for_pat() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(BodyRpc("initialize"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream broken"))
        .mount(&server)
        .await;
    let f = fixture();
    let manager = f.manager("test-agent");
    let err = manager
        .install("broken", pat_config(&server.uri()), InstalledVia::Agent)
        .await
        .unwrap_err();
    // PAT auth has no "needs credentials" branch — a real failure
    // surfaces as Err, the store row is still written so the next
    // retry has consistent state, but no tools / registry entry.
    assert!(err.to_string().contains("initialize"), "{err}");
    assert!(f.tools.names().is_empty());
    let row = f.store.get("test-agent", "broken").unwrap();
    assert!(
        row.is_some(),
        "store row is persisted even when spawn fails so retry is consistent"
    );
}

#[tokio::test]
async fn authorize_start_rejects_pat_servers() {
    let server = MockServer::start().await;
    mount_initialize_and_tools(
        &server,
        json!({"tools": [{"name": "echo", "description": "", "inputSchema": {"type": "object"}}]}),
    )
    .await;
    let f = fixture();
    let manager = f.manager("test-agent");
    manager
        .install("alpha", pat_config(&server.uri()), InstalledVia::Agent)
        .await
        .unwrap();
    let err = manager.authorize_start("alpha", "kings").await.unwrap_err();
    assert!(
        err.to_string().contains("PAT auth"),
        "PAT servers don't need OAuth; {err}"
    );
}

#[tokio::test]
async fn authorize_start_returns_url_and_persists_state_for_oauth() {
    let f = fixture();
    // Seed OAuth client creds — operator-supplied app credentials.
    f.vault
        .set("mcp/v1/linear/client_id", "lin_client")
        .unwrap();
    f.vault
        .set("mcp/v1/linear/client_secret", "lin_secret")
        .unwrap();
    f.vault
        .set(
            "mcp/v1/linear/redirect_uri",
            "https://hub.example/api/auth/linear/callback",
        )
        .unwrap();

    let cfg = McpServerConfig {
        endpoint: "https://mcp.linear.app/mcp".to_string(),
        transport: "streamable-http".to_string(),
        auth: McpAuthConfig::Oauth2 {
            binding: workforce_common::settings::McpAuthBinding::User,
            provider: "linear".to_string(),
            scopes: vec!["read".to_string()],
            shared_pat_fallback: None,
            discovery: None,
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
    };
    // Persist as if we'd already called install (and got a
    // NeedsCredentials response).
    let cfg_value = serde_json::to_value(&cfg).unwrap();
    let record = workforce_mcp_store::McpServerRecord::new(
        "test-agent",
        "linear",
        cfg_value,
        InstalledVia::Agent,
    );
    f.store.put(&record).unwrap();

    let manager = f.manager("test-agent");
    let start = manager.authorize_start("linear", "kings").await.unwrap();
    assert_eq!(start.server_id, "linear");
    assert_eq!(start.provider, "linear");
    assert!(start.authorization_url.contains("linear.app"));
    assert!(start.authorization_url.contains("client_id=lin_client"));
    assert!(start.authorization_url.contains("response_type=code"));

    // The state should be persisted in the OAuthStateStore.
    let url = url::Url::parse(&start.authorization_url).unwrap();
    let state_token = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    let entry = f.oauth_state.take(&state_token).unwrap().unwrap();
    assert!(entry.provider.contains("linear"));
    assert_eq!(entry.uid, "kings");
    assert_eq!(entry.workspace, "test-agent");
}

#[tokio::test]
async fn install_oauth_without_creds_returns_needs_credentials() {
    let f = fixture();
    let cfg = McpServerConfig {
        endpoint: "https://mcp.linear.app/mcp".to_string(),
        transport: "streamable-http".to_string(),
        auth: McpAuthConfig::Oauth2 {
            binding: workforce_common::settings::McpAuthBinding::User,
            provider: "linear".to_string(),
            scopes: Vec::new(),
            shared_pat_fallback: None,
            discovery: None,
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
    };
    let manager = f.manager("test-agent");
    let report = manager
        .install("linear", cfg, InstalledVia::Agent)
        .await
        .unwrap();
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["status"], "needs_credentials");
    let missing = value["missing"].as_array().unwrap();
    assert!(missing
        .iter()
        .any(|m| m.as_str().unwrap().ends_with("client_id")));
    // Store row is persisted so authorize_complete can find it later.
    assert!(f.store.get("test-agent", "linear").unwrap().is_some());
}

#[tokio::test]
async fn server_statuses_reports_installed_but_unauthorised_as_needs_auth() {
    // The round-1 bug closer: a server can be INSTALLED (store row) yet never
    // connected (registry) — the transcript's "it's configured, just needs
    // authorising" state. `set_member` only fires on a successful spawn, so this
    // server is NOT in the workspace MEMBERS registry either. `server_statuses()`
    // must still surface it (from the store) as `needs_auth`, NOT report it
    // absent — which is exactly what the env state board relies on.
    let f = fixture();
    let cfg = McpServerConfig {
        endpoint: "https://mcp.linear.app/mcp".to_string(),
        transport: "streamable-http".to_string(),
        auth: McpAuthConfig::Oauth2 {
            binding: workforce_common::settings::McpAuthBinding::User,
            provider: "linear".to_string(),
            scopes: Vec::new(),
            shared_pat_fallback: None,
            discovery: None,
        },
        allowed_hosts: Vec::new(),
        refresh_interval_secs: 3600,
        shadows: Vec::new(),
        fallback_to_builtin: false,
        tools_allow: Vec::new(),
        tools_deny: Vec::new(),
        workspace: Some("harbor".to_string()),
        env: Some("staging".to_string()),
        default: false,
    };
    let manager = f.manager("test-agent");
    // OAuth install with no client creds → store row persisted, spawn fails →
    // NOT in the registry (and not in MEMBERS).
    let report = manager
        .install("harbor-staging", cfg, InstalledVia::Agent)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&report).unwrap()["status"],
        "needs_credentials"
    );

    let statuses = manager.server_statuses();
    let s = statuses
        .iter()
        .find(|s| s.server_id == "harbor-staging")
        .expect("installed server must appear in server_statuses, not be reported absent");
    assert_eq!(s.workspace.as_deref(), Some("harbor"));
    assert_eq!(s.env.as_deref(), Some("staging"));
    assert!(
        !s.connected,
        "spawn failed on missing creds → not connected"
    );
    assert!(
        s.needs_auth,
        "OAuth + installed + not connected → needs authorisation"
    );
}

#[tokio::test]
async fn authorize_complete_persists_tokens_and_spawns_server() {
    use async_trait::async_trait;
    use workforce_oauth_client::OAuthProvider;

    // Wiremock-faked vendor: serves both the OAuth token endpoint
    // (for `exchange_web_code`) and the MCP server (initialise +
    // tools/list). Co-locating them in one mock server keeps the
    // test self-contained.
    let vendor = MockServer::start().await;

    // OAuth token endpoint — answers `exchange_web_code` with a
    // synthetic refresh + access token.
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-fresh",
            "refresh_token": "rt-fresh",
            "expires_in": 3600,
            "scope": "read"
        })))
        .mount(&vendor)
        .await;

    // MCP server: standard initialise + tools/list. Same path the
    // earlier tests use.
    mount_initialize_and_tools(
        &vendor,
        json!({"tools": [{"name": "echo", "description": "", "inputSchema": {"type": "object"}}]}),
    )
    .await;

    // Test-only OAuth provider whose endpoints point at the
    // wiremock instance. Registered process-wide; cleaned up at the
    // end of the test so it doesn't bleed into siblings.
    struct TestProvider {
        token: String,
        authorize: String,
    }
    #[async_trait]
    impl OAuthProvider for TestProvider {
        fn name(&self) -> &str {
            "linear-test"
        }
        fn authorization_endpoint(&self) -> &str {
            &self.authorize
        }
        fn token_endpoint(&self) -> &str {
            &self.token
        }
        fn default_scopes(&self) -> &[&str] {
            &["read"]
        }
    }
    let provider_name = format!("linear-test-{}", uuid::Uuid::new_v4());
    workforce_hub::mcp_servers::oauth_bridge::register_test_provider(
        &provider_name,
        Arc::new(TestProvider {
            token: format!("{}/oauth/token", vendor.uri()),
            authorize: format!("{}/oauth/authorize", vendor.uri()),
        }),
    );

    let f = fixture();
    f.vault
        .set("mcp/v1/linearx/client_id", "lin_client")
        .unwrap();
    f.vault
        .set("mcp/v1/linearx/client_secret", "lin_secret")
        .unwrap();
    f.vault
        .set(
            "mcp/v1/linearx/redirect_uri",
            "https://hub.example/api/auth/linear/callback",
        )
        .unwrap();

    let cfg = McpServerConfig {
        endpoint: vendor.uri(), // MCP base — wiremock serves "/" too
        transport: "streamable-http".to_string(),
        auth: McpAuthConfig::Oauth2 {
            binding: workforce_common::settings::McpAuthBinding::User,
            provider: provider_name.clone(),
            scopes: vec!["read".to_string()],
            shared_pat_fallback: None,
            discovery: None,
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
    };
    // Persist the install (as if `install` had returned NeedsCredentials).
    let cfg_value = serde_json::to_value(&cfg).unwrap();
    let record = workforce_mcp_store::McpServerRecord::new(
        "test-agent",
        "linearx",
        cfg_value,
        InstalledVia::Agent,
    );
    f.store.put(&record).unwrap();

    let manager = f.manager("test-agent");
    let start = manager.authorize_start("linearx", "kings").await.unwrap();
    let url = url::Url::parse(&start.authorization_url).unwrap();
    let state_token = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();

    // The real OAuth callback runs with no ambient `CURRENT_USER_ID` (a
    // browser redirect, not an agent turn). Calling `authorize_complete`
    // unscoped here guards that `complete_with_entry` scopes the post-auth
    // spawn to `entry.uid` itself — else the binding=User `tools/list`
    // fails and the `mcp.linearx.echo` assertion below never registers.
    let server_id = manager
        .authorize_complete(&state_token, "auth-code-123")
        .await
        .unwrap();
    assert_eq!(server_id, "linearx");

    // Tokens persisted under the per-user path (binding=User).
    assert_eq!(
        f.vault
            .get("users/kings/mcp/v1/linearx/access_token")
            .as_deref(),
        Some("at-fresh")
    );
    assert_eq!(
        f.vault
            .get("users/kings/mcp/v1/linearx/refresh_token")
            .as_deref(),
        Some("rt-fresh")
    );
    // Server is now spawned and tools registered.
    assert!(f.tools.get("mcp.linearx.echo").is_some());
    assert_eq!(manager.list().len(), 1);

    workforce_hub::mcp_servers::oauth_bridge::unregister_test_provider(&provider_name);
}

/// The real target of `set_tool_filter` is an OAuth/discovery server like
/// Harbor: curating its surface must respawn from the *stored* config
/// (resolved endpoint, cached metadata, vault tokens) with no
/// re-authorization. Because the server is `binding = User`, the respawn's
/// `tools/list` resolves the *calling user's* token, so the call must run
/// inside that user's `CURRENT_USER_ID` scope — exactly how an agent turn
/// invokes it.
#[tokio::test]
async fn set_tool_filter_hot_swaps_oauth_server_without_reauth() {
    use async_trait::async_trait;
    use workforce_oauth_client::OAuthProvider;

    let vendor = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-fresh",
            "refresh_token": "rt-fresh",
            "expires_in": 3600,
            "scope": "read"
        })))
        .mount(&vendor)
        .await;
    mount_initialize_and_tools(&vendor, three_tool_server_body()).await;

    struct TestProvider {
        token: String,
        authorize: String,
    }
    #[async_trait]
    impl OAuthProvider for TestProvider {
        fn name(&self) -> &str {
            "linear-test"
        }
        fn authorization_endpoint(&self) -> &str {
            &self.authorize
        }
        fn token_endpoint(&self) -> &str {
            &self.token
        }
        fn default_scopes(&self) -> &[&str] {
            &["read"]
        }
    }
    let provider_name = format!("linear-test-{}", uuid::Uuid::new_v4());
    workforce_hub::mcp_servers::oauth_bridge::register_test_provider(
        &provider_name,
        Arc::new(TestProvider {
            token: format!("{}/oauth/token", vendor.uri()),
            authorize: format!("{}/oauth/authorize", vendor.uri()),
        }),
    );

    let f = fixture();
    f.vault.set("mcp/v1/acme/client_id", "cid").unwrap();
    f.vault.set("mcp/v1/acme/client_secret", "csec").unwrap();
    f.vault
        .set("mcp/v1/acme/redirect_uri", "https://hub.example/cb")
        .unwrap();

    let cfg = McpServerConfig {
        endpoint: vendor.uri(),
        transport: "streamable-http".to_string(),
        auth: McpAuthConfig::Oauth2 {
            binding: workforce_common::settings::McpAuthBinding::User,
            provider: provider_name.clone(),
            scopes: vec!["read".to_string()],
            shared_pat_fallback: None,
            discovery: None,
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
    };
    let record = workforce_mcp_store::McpServerRecord::new(
        "test-agent",
        "acme",
        serde_json::to_value(&cfg).unwrap(),
        InstalledVia::Agent,
    );
    f.store.put(&record).unwrap();

    let manager = f.manager("test-agent");
    let start = manager.authorize_start("acme", "kings").await.unwrap();
    let state_token = url::Url::parse(&start.authorization_url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    manager
        .authorize_complete(&state_token, "code-123")
        .await
        .unwrap();
    // Full OAuth surface is live after authorization.
    assert!(f.tools.get("mcp.acme.issue_create").is_some());
    assert!(f.tools.get("mcp.acme.comment_list").is_some());

    // Curate it — under the calling user's scope, as an agent turn would.
    let report = workforce_agent::runner::CURRENT_USER_ID
        .scope(
            "kings".to_string(),
            manager.set_tool_filter(
                "acme",
                vec!["issue_*".to_string()],
                vec!["issue_delete".to_string()],
            ),
        )
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(&report).unwrap()["tool_count"], 1);
    assert!(f.tools.get("mcp.acme.issue_create").is_some());
    assert!(f.tools.get("mcp.acme.issue_delete").is_none());
    assert!(f.tools.get("mcp.acme.comment_list").is_none());

    // No re-authorization happened: the persisted access token is the same
    // one minted at authorize-time, untouched by the hot-swap.
    assert_eq!(
        f.vault
            .get("users/kings/mcp/v1/acme/access_token")
            .as_deref(),
        Some("at-fresh")
    );

    workforce_hub::mcp_servers::oauth_bridge::unregister_test_provider(&provider_name);
}

#[tokio::test]
async fn over_cap_server_registers_only_allowed_subset() {
    let server = MockServer::start().await;
    // 600 tools — over the 512 registration cap raw: 10 `keep_*`, 590 `bulk_*`.
    let tools: Vec<Value> = (0..600)
        .map(|i| {
            let name = if i < 10 {
                format!("keep_{i}")
            } else {
                format!("bulk_{i}")
            };
            json!({"name": name, "description": "", "inputSchema": {"type": "object"}})
        })
        .collect();
    mount_initialize_and_tools(&server, json!({"tools": tools})).await;

    let f = fixture();
    let manager = f.manager("test-agent");

    // No allowlist → the over-cap fetch fails with the curation hint.
    let err = manager
        .install("test", pat_config(&server.uri()), InstalledVia::Agent)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("mcp.list_remote_tools"),
        "expected curation hint, got: {err}"
    );

    // Allowlist `keep_*` → the cap counts only the 10 kept tools, so it
    // registers cleanly even though the server advertises 600.
    let mut cfg = pat_config(&server.uri());
    cfg.tools_allow = vec!["keep_*".to_string()];
    let report = manager
        .install("test", cfg, InstalledVia::Agent)
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(&report).unwrap()["tool_count"], 10);
    assert!(f.tools.get("mcp.test.keep_0").is_some());
    assert!(f.tools.get("mcp.test.bulk_100").is_none());
}

#[tokio::test]
async fn probe_remote_tools_summarizes_families_without_registering() {
    let server = MockServer::start().await;
    let tools: Vec<Value> = [
        "order_create",
        "order_cancel",
        "order_list",
        "worker_get",
        "worker_list",
        "booking_make",
    ]
    .iter()
    .map(|n| json!({"name": n, "description": format!("{n} desc"), "inputSchema": {"type": "object"}}))
    .collect();
    mount_initialize_and_tools(&server, json!({"tools": tools})).await;

    let f = fixture();
    // Persist a record WITHOUT installing — the dormant-server case (e.g.
    // a server that failed to hydrate over-cap). The probe reads the store.
    let record = workforce_mcp_store::McpServerRecord::new(
        "test-agent",
        "acme",
        serde_json::to_value(pat_config(&server.uri())).unwrap(),
        InstalledVia::Agent,
    );
    f.store.put(&record).unwrap();
    let manager = f.manager("test-agent");

    // Summary view: families sorted by count, nothing registered.
    let survey = manager.probe_remote_tools("acme", None).await.unwrap();
    assert_eq!(survey.total_tools, 6);
    assert!(!survey.truncated);
    assert_eq!(survey.family_count, 3);
    assert_eq!(survey.families[0].prefix, "order");
    assert_eq!(survey.families[0].count, 3);
    assert!(survey.matched.is_empty());
    assert!(
        f.tools.get("mcp.acme.order_create").is_none(),
        "probe must not register anything"
    );

    // Drill into the `order` family.
    let drill = manager
        .probe_remote_tools("acme", Some("order"))
        .await
        .unwrap();
    let names: Vec<&str> = drill.matched.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["order_create", "order_cancel", "order_list"]);
}

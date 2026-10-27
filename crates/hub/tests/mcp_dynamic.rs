//! End-to-end tests for `McpServerManager::install_dynamic` — the
//! self-hosted OAuth path (RFC 8414 discovery + RFC 7591 dynamic client
//! registration) used by the Metabase preset.
//!
//! A single wiremock server fakes the instance: the two `.well-known`
//! discovery documents, the registration endpoint, and the token
//! endpoint. The MCP endpoint itself is left unmounted — with no user
//! token the spawn yields `NeedsCredentials` (ready to authorize)
//! before any MCP request is sent.

use std::sync::Arc;

use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use workforce_common::settings::{McpAuthBinding, McpAuthConfig, McpServerConfig};
use workforce_hub::mcp_servers::{InstallReport, McpServerManager};
use workforce_hub::vault::Vault;
use workforce_mcp_store::{InstalledVia, McpServerStore};
use workforce_oauth_client::{InMemoryOAuthStateStore, OAuthStateStore};

const SITE_URL_PATH: &str = "skills/metabase/site_url";
const PUBLIC_URL: &str = "https://hub.example";

struct Fixture {
    _tmp: TempDir,
    store: Arc<dyn McpServerStore>,
    vault: Arc<Vault>,
    manager: Arc<McpServerManager>,
}

fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let vault = Arc::new(Vault::open(&tmp.path().join("vault.json"), "test-key").unwrap());
    let store: Arc<dyn McpServerStore> =
        Arc::new(workforce_mcp_store::sqlite::SqliteMcpServerStore::in_memory().unwrap());
    let oauth_state: Arc<dyn OAuthStateStore> = Arc::new(InMemoryOAuthStateStore::new());
    let manager = McpServerManager::new(
        "agent",
        Arc::clone(&store),
        Arc::new(workforce_agent::tools::registry::ToolRegistry::new()),
        Some(Arc::clone(&vault)),
        Some(Arc::clone(&vault)),
        oauth_state,
        None,
        Arc::new(reqwest::Client::new()),
    );
    Fixture {
        _tmp: tmp,
        store,
        vault,
        manager,
    }
}

fn dynamic_config() -> McpServerConfig {
    McpServerConfig {
        endpoint: "{base_url}/api/mcp".to_string(),
        transport: "streamable-http".to_string(),
        auth: McpAuthConfig::Oauth2 {
            binding: McpAuthBinding::User,
            provider: "metabase".to_string(),
            scopes: Vec::new(),
            shared_pat_fallback: None,
            discovery: None,
        },
        allowed_hosts: Vec::new(),
        refresh_interval_secs: 3600,
        shadows: vec!["metabase.*".to_string()],
        fallback_to_builtin: false,
        tools_allow: Vec::new(),
        tools_deny: Vec::new(),
        workspace: None,
        env: None,
        default: false,
    }
}

/// Mounts discovery (protected-resource → auth-server), registration,
/// and token endpoints, all rooted at the wiremock origin. The
/// registration endpoint returns `client_id` (and `client_secret` when
/// given), so distinct mock instances can hand out distinct clients.
async fn mount_instance(server: &MockServer, client_id: &str, client_secret: Option<&str>) {
    let issuer = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "authorization_servers": [issuer] })),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_endpoint": format!("{issuer}/auth"),
            "token_endpoint": format!("{issuer}/token"),
            "registration_endpoint": format!("{issuer}/register"),
            "scopes_supported": [],
        })))
        .mount(server)
        .await;
    let mut registered = json!({ "client_id": client_id });
    if let Some(secret) = client_secret {
        registered["client_secret"] = json!(secret);
    }
    Mock::given(method("POST"))
        .and(path("/register"))
        .respond_with(ResponseTemplate::new(201).set_body_json(registered))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-1",
            "refresh_token": "rt-1",
            "expires_in": 3600,
            "scope": "",
        })))
        .mount(server)
        .await;
}

fn register_hits(reqs: &[wiremock::Request]) -> usize {
    reqs.iter()
        .filter(|r| r.url.path().ends_with("/register"))
        .count()
}

/// Like `mount_instance` but the authorization server advertises NO
/// `registration_endpoint` — models a standard IdP (Keycloak / Okta /
/// Auth0) where OAuth clients are registered out-of-band.
async fn mount_instance_no_registration(server: &MockServer) {
    let issuer = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "authorization_servers": [issuer] })),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_endpoint": format!("{issuer}/auth"),
            "token_endpoint": format!("{issuer}/token"),
            "scopes_supported": [],
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-1",
            "refresh_token": "rt-1",
            "expires_in": 3600,
            "scope": "",
        })))
        .mount(server)
        .await;
}

/// A fixed-endpoint OAuth2 config — the shape produced by `mcp.install`
/// `custom` with `discover: true` (no `{base_url}` template).
fn discover_config(endpoint: String, provider: &str) -> McpServerConfig {
    McpServerConfig {
        endpoint,
        transport: "streamable-http".to_string(),
        auth: McpAuthConfig::Oauth2 {
            binding: McpAuthBinding::User,
            provider: provider.to_string(),
            scopes: vec!["openid".to_string()],
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
    }
}

#[tokio::test]
async fn install_dynamic_discovers_registers_and_caches() {
    let server = MockServer::start().await;
    mount_instance(&server, "dyn-cid", None).await;
    let f = fixture();
    f.vault.set(SITE_URL_PATH, &server.uri()).unwrap();

    let report = f
        .manager
        .install_dynamic(
            "metabase",
            dynamic_config(),
            Some(SITE_URL_PATH),
            Some(PUBLIC_URL),
            None,
            InstalledVia::Api,
        )
        .await
        .unwrap();

    // No user token yet → ready-to-authorize, not a hard failure.
    match report {
        InstallReport::NeedsCredentials { missing, .. } => {
            assert!(missing.is_empty(), "client is registered; nothing missing");
        }
        other => panic!("expected NeedsCredentials, got {other:?}"),
    }

    // Public client: id + redirect persisted, no secret.
    assert_eq!(
        f.vault.get("mcp/v1/metabase/client_id").as_deref(),
        Some("dyn-cid")
    );
    assert_eq!(
        f.vault.get("mcp/v1/metabase/redirect_uri").as_deref(),
        Some("https://hub.example/api/auth/metabase/callback")
    );
    assert!(f.vault.get("mcp/v1/metabase/client_secret").is_none());

    // Discovered metadata is cached on the persisted config.
    let record = f.store.get("agent", "metabase").unwrap().unwrap();
    let cfg: McpServerConfig = serde_json::from_value(record.config).unwrap();
    let McpAuthConfig::Oauth2 { discovery, .. } = cfg.auth else {
        panic!("expected OAuth2");
    };
    let meta = discovery.expect("discovery cached");
    assert_eq!(
        meta.authorization_endpoint,
        format!("{}/auth", server.uri())
    );
    assert_eq!(meta.token_endpoint, format!("{}/token", server.uri()));
}

#[tokio::test]
async fn reinstall_rediscovers_but_does_not_reregister() {
    let server = MockServer::start().await;
    mount_instance(&server, "dyn-cid", None).await;
    let f = fixture();
    f.vault.set(SITE_URL_PATH, &server.uri()).unwrap();

    for _ in 0..2 {
        f.manager
            .install_dynamic(
                "metabase",
                dynamic_config(),
                Some(SITE_URL_PATH),
                Some(PUBLIC_URL),
                None,
                InstalledVia::Api,
            )
            .await
            .unwrap();
    }

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        register_hits(&reqs),
        1,
        "registration must be idempotent once a client_id is stored"
    );
}

#[tokio::test]
async fn reinstall_against_new_issuer_reregisters() {
    let f = fixture();

    let server_a = MockServer::start().await;
    mount_instance(&server_a, "client-a", None).await;
    f.vault.set(SITE_URL_PATH, &server_a.uri()).unwrap();
    f.manager
        .install_dynamic(
            "metabase",
            dynamic_config(),
            Some(SITE_URL_PATH),
            Some(PUBLIC_URL),
            None,
            InstalledVia::Api,
        )
        .await
        .unwrap();
    assert_eq!(
        f.vault.get("mcp/v1/metabase/client_id").as_deref(),
        Some("client-a")
    );

    // Repoint the instance at a different authorization server.
    let server_b = MockServer::start().await;
    mount_instance(&server_b, "client-b", None).await;
    f.vault.set(SITE_URL_PATH, &server_b.uri()).unwrap();
    f.manager
        .install_dynamic(
            "metabase",
            dynamic_config(),
            Some(SITE_URL_PATH),
            Some(PUBLIC_URL),
            None,
            InstalledVia::Api,
        )
        .await
        .unwrap();

    assert_eq!(
        f.vault.get("mcp/v1/metabase/client_id").as_deref(),
        Some("client-b"),
        "a moved issuer must trigger re-registration against the new server"
    );
    assert_eq!(
        register_hits(&server_b.received_requests().await.unwrap()),
        1
    );
}

#[tokio::test]
async fn install_dynamic_without_base_url_persists_nothing() {
    let server = MockServer::start().await;
    mount_instance(&server, "dyn-cid", None).await;
    let f = fixture();
    // site_url intentionally unset.

    let report = f
        .manager
        .install_dynamic(
            "metabase",
            dynamic_config(),
            Some(SITE_URL_PATH),
            Some(PUBLIC_URL),
            None,
            InstalledVia::Api,
        )
        .await
        .unwrap();

    match report {
        InstallReport::NeedsCredentials { missing, .. } => {
            assert_eq!(missing, vec![SITE_URL_PATH.to_string()]);
        }
        other => panic!("expected NeedsCredentials, got {other:?}"),
    }
    assert!(
        f.store.get("agent", "metabase").unwrap().is_none(),
        "no row may be persisted before the instance URL is known"
    );
    assert!(f.vault.get("mcp/v1/metabase/client_id").is_none());
    assert!(
        register_hits(&server.received_requests().await.unwrap()) == 0,
        "no registration before the instance URL is known"
    );
}

#[tokio::test]
async fn authorize_uses_discovered_endpoints_and_persists_tokens() {
    let server = MockServer::start().await;
    mount_instance(&server, "dyn-cid", None).await;
    let f = fixture();
    f.vault.set(SITE_URL_PATH, &server.uri()).unwrap();
    f.manager
        .install_dynamic(
            "metabase",
            dynamic_config(),
            Some(SITE_URL_PATH),
            Some(PUBLIC_URL),
            None,
            InstalledVia::Api,
        )
        .await
        .unwrap();

    let start = f.manager.authorize_start("metabase", "u1").await.unwrap();
    assert!(
        start
            .authorization_url
            .starts_with(&format!("{}/auth", server.uri())),
        "authorize URL must use the discovered endpoint: {}",
        start.authorization_url
    );
    assert!(start.authorization_url.contains("client_id=dyn-cid"));

    let state = url::Url::parse(&start.authorization_url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .expect("state param present");

    f.manager
        .authorize_complete(&state, "auth-code")
        .await
        .unwrap();

    assert_eq!(
        f.vault
            .get("users/u1/mcp/v1/metabase/refresh_token")
            .as_deref(),
        Some("rt-1"),
        "exchanged tokens land at the per-user path"
    );
}

/// An operator-registered client (seeded out-of-band) is reused: discovery
/// still resolves the endpoints, but registration is skipped entirely — even
/// though the server advertises a registration endpoint. This is the path a
/// `custom` + `discover: true` install of a Keycloak-fronted server takes.
#[tokio::test]
async fn install_dynamic_reuses_preseeded_client_and_skips_registration() {
    let server = MockServer::start().await;
    mount_instance(&server, "self-registered-cid", None).await;
    let f = fixture();
    // Operator pre-registered a client at the IdP and seeded its id.
    f.vault
        .set("mcp/v1/harbor/client_id", "operator-cid")
        .unwrap();

    let report = f
        .manager
        .install_dynamic(
            "harbor",
            discover_config(format!("{}/mcp", server.uri()), "harbor"),
            None,
            Some(PUBLIC_URL),
            None,
            InstalledVia::Agent,
        )
        .await
        .unwrap();

    // Client present, no user token yet → ready to authorize.
    match report {
        InstallReport::NeedsCredentials { missing, .. } => {
            assert!(missing.is_empty(), "client is seeded; nothing missing");
        }
        other => panic!("expected NeedsCredentials (ready to authorize), got {other:?}"),
    }
    // The seeded client is kept verbatim; the registration endpoint is untouched.
    assert_eq!(
        f.vault.get("mcp/v1/harbor/client_id").as_deref(),
        Some("operator-cid")
    );
    assert_eq!(
        register_hits(&server.received_requests().await.unwrap()),
        0,
        "a seeded client must never trigger registration"
    );
    // The derived redirect + discovered endpoints are persisted for authorize.
    assert_eq!(
        f.vault.get("mcp/v1/harbor/redirect_uri").as_deref(),
        Some("https://hub.example/api/auth/harbor/callback")
    );
    let record = f.store.get("agent", "harbor").unwrap().unwrap();
    let cfg: McpServerConfig = serde_json::from_value(record.config).unwrap();
    let McpAuthConfig::Oauth2 { discovery, .. } = cfg.auth else {
        panic!("expected OAuth2");
    };
    assert_eq!(
        discovery.expect("discovery cached").authorization_endpoint,
        format!("{}/auth", server.uri())
    );
}

/// An IdP that neither advertises registration nor has a seeded client yields
/// an actionable `NeedsCredentials` (register a client, seed `client_id`) —
/// not a hard error — and persists nothing.
#[tokio::test]
async fn install_dynamic_without_registration_or_client_asks_to_register() {
    let server = MockServer::start().await;
    mount_instance_no_registration(&server).await;
    let f = fixture();

    let report = f
        .manager
        .install_dynamic(
            "harbor",
            discover_config(format!("{}/mcp", server.uri()), "harbor"),
            None,
            Some(PUBLIC_URL),
            None,
            InstalledVia::Agent,
        )
        .await
        .unwrap();

    match report {
        InstallReport::NeedsCredentials {
            missing,
            instructions,
            ..
        } => {
            assert_eq!(missing, vec!["mcp/v1/harbor/client_id".to_string()]);
            assert!(
                instructions.contains("https://hub.example/api/auth/harbor/callback"),
                "instructions must embed the redirect URI to register: {instructions}"
            );
        }
        other => panic!("expected NeedsCredentials, got {other:?}"),
    }
    assert!(
        f.store.get("agent", "harbor").unwrap().is_none(),
        "nothing is persisted until a client is registered"
    );
    assert!(f.vault.get("mcp/v1/harbor/client_id").is_none());
}

/// Keycloak pattern: the MCP API and the OIDC issuer are different hosts.
/// `discovery_issuer` must steer discovery to the issuer, not the endpoint.
#[tokio::test]
async fn discovery_issuer_overrides_mcp_endpoint() {
    let issuer = MockServer::start().await;
    mount_instance(&issuer, "kc-cid", None).await;
    // A stand-in MCP endpoint that serves NO .well-known docs.
    let mcp = MockServer::start().await;
    let f = fixture();

    let mut config = dynamic_config();
    config.endpoint = format!("{}/mcp", mcp.uri());

    // With the issuer override → discovery + registration succeed against
    // the issuer, leaving the server ready to authorize.
    let report = f
        .manager
        .install_dynamic(
            "kc",
            config.clone(),
            None,
            Some(PUBLIC_URL),
            Some(&issuer.uri()),
            InstalledVia::Agent,
        )
        .await
        .unwrap();
    assert!(
        matches!(report, InstallReport::NeedsCredentials { .. }),
        "expected ready-to-authorize, got {report:?}"
    );
    assert_eq!(
        f.vault.get("mcp/v1/kc/client_id").as_deref(),
        Some("kc-cid")
    );

    // Without it → discovery hits the doc-less MCP endpoint and fails,
    // proving the issuer override is what made the first call work.
    let err = f
        .manager
        .install_dynamic(
            "kc2",
            config,
            None,
            Some(PUBLIC_URL),
            None,
            InstalledVia::Agent,
        )
        .await;
    assert!(
        err.is_err(),
        "discovery against an endpoint with no .well-known must fail"
    );
}

const SECRETS: [&str; 5] = ["opsuser7", "hunter2", "k-northwind42", "s3cr3t", "fr4gment"];

fn secret_endpoint(addr: impl std::fmt::Display) -> String {
    format!("http://opsuser7:hunter2@{addr}/mcp/k-northwind42?sig=s3cr3t#fr4gment")
}

#[tokio::test]
async fn discovery_failure_names_no_endpoint_secret() {
    let server = MockServer::start().await;
    let f = fixture();

    let err = f
        .manager
        .install_dynamic(
            "acme",
            discover_config(secret_endpoint(server.address()), "acme"),
            None,
            Some(PUBLIC_URL),
            None,
            InstalledVia::Api,
        )
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("OAuth discovery failed"), "{text}");
    assert!(text.contains(&server.address().to_string()), "{text}");
    for secret in SECRETS {
        assert!(!text.contains(secret), "`{secret}` leaked: {text}");
    }

    let err = f
        .manager
        .install_dynamic(
            "acme",
            discover_config(secret_endpoint(server.address()), "acme"),
            None,
            Some(PUBLIC_URL),
            Some("https://opsuser7:hunter2@[bad"),
            InstalledVia::Api,
        )
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("<unparseable url>"), "{text}");
    for secret in SECRETS {
        assert!(!text.contains(secret), "`{secret}` leaked: {text}");
    }
}

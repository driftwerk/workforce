//! Gateways and hub state with real identity, JWT and agent registries and
//! nothing else, for route tests that need a signed-in caller.

use std::collections::HashMap;
use std::sync::Arc;
use workforce_hub::serve::Compose as _;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use workforce_hub::agent_registry::AgentRegistry;
use workforce_hub::external_skills::ExternalSkills;
use workforce_hub::gateway::GatewayState;
use workforce_hub::jwt::{generate_token_with_identity, refresh_token, Claims, JwtConfig};
use workforce_hub::serve::AgentService;
use workforce_hub::skill_catalog::SkillCatalog;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};
use workforce_orchestrator::workflow_store::WorkflowStore;

pub const GATEWAY_TOKEN: &str = "test-gateway-owner-token";
const JWT_SECRET: &str = "test-secret-key-at-least-32-bytes-long!!";

/// The JWT config every test gateway signs and checks tokens with.
pub fn jwt() -> JwtConfig {
    JwtConfig::new(JWT_SECRET.to_string(), 3600).expect("jwt config")
}

/// What a test gateway is made of: the owner token, [`jwt`], `identities`
/// and `registry`, plus whichever optional services a test wires; every other
/// service is off.
pub struct GatewayParts {
    pub registry: AgentRegistry,
    pub identities: Arc<dyn IdentityStore>,
    pub skills_dir: std::path::PathBuf,
    pub def_store: Option<Arc<WorkflowStore>>,
    pub vault: Option<Arc<workforce_hub::vault::Vault>>,
    pub telemetry_store: Option<Arc<dyn workforce_common::telemetry::TelemetryStore>>,
    pub device_pairing: Option<Arc<workforce_pairing::DevicePairingService>>,
    pub monitor_engine: Option<Arc<workforce_hub::monitor::MonitorEngine>>,
    pub destructive_baseline_active: bool,
    /// The base config file the overlay writes beside.
    pub config_path: Option<std::path::PathBuf>,
}

impl GatewayParts {
    pub fn new(
        registry: AgentRegistry,
        identities: Arc<dyn IdentityStore>,
        skills_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            registry,
            identities,
            skills_dir,
            def_store: None,
            vault: None,
            telemetry_store: None,
            device_pairing: None,
            monitor_engine: None,
            destructive_baseline_active: false,
            config_path: None,
        }
    }

    pub fn state(self) -> Arc<GatewayState> {
        let (broadcast_tx, _rx) = tokio::sync::broadcast::channel(16);
        let external_skills = ExternalSkills::shared(&self.skills_dir);
        Arc::new(GatewayState {
            registry: Arc::new(self.registry),
            nats: None,
            auth_token: GATEWAY_TOKEN.to_string(),
            broadcast_tx,
            progress_forwarders: Default::default(),
            channel_pairing: None,
            device_pairing: self.device_pairing,
            sentinel: Default::default(),
            access_gate: workforce_hub::access_gate::AccessGate::new(HashMap::new()),
            drain_token: tokio_util::sync::CancellationToken::new(),
            node_invoke_pending: Arc::default(),
            orchestrator: None,
            local_agents: Arc::new(workforce_hub::local_agent_registry::LocalAgentRegistry::new()),
            voice_listeners: Arc::new(workforce_hub::voice_announce::VoiceListeners::default()),
            vault: self.vault,
            artifact_store: None,
            def_store: self.def_store,
            overlay_kick: None,
            workflow_runtime: None,
            playbook_runtime: None,
            consolidation_runtime: None,
            monitor_engine: self
                .monitor_engine
                .map(|engine| engine as Arc<dyn workforce_hub::hosts::MonitorHost>),
            engagement_overrides: Arc::new(workforce_common::engagement::EngagementOverrides::new()),
            config_path: self.config_path,
            identity_store: Some(self.identities),
            team_store: None,
            remote_mcp_tokens: None,
            remote_mcp: Default::default(),
            permission_evaluator: None,
            destructive_baseline_active: self.destructive_baseline_active,
            telemetry_store: self.telemetry_store,
            jwt_config: Some(jwt()),
            rate_limiter: None,
            pair_rate_limiter: None,
            action_limiter: None,
            trusted_proxies: Arc::new(
                workforce_hub::trusted_proxy::TrustedProxyConfig::from_strings(&[]).unwrap(),
            ),
            skills_dir: self.skills_dir,
            slack: Default::default(),
            skill_catalog: Arc::new(SkillCatalog::live(
                &Default::default(),
                None,
                Arc::clone(&external_skills),
            )),
            external_skills,
            #[cfg(feature = "oidc")]
            oidc_provider: None,
        })
    }
}

/// `state`'s gateway on a loopback port, as `workforce serve` runs it.
pub async fn serve(state: Arc<GatewayState>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            workforce_hub::gateway::router(state)
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

/// A JWT for `identity`, as a real sign-in mints it.
pub fn token_for(identity: &Identity) -> String {
    generate_token_with_identity(
        &jwt(),
        &identity.display_name,
        identity.role,
        Some(identity.id.to_string()),
    )
    .unwrap()
}

/// A JWT holding every scope, `admin` included, for `identity`, as the
/// legacy-wildcard rollback mints one for any role.
pub fn wildcard_token_for(identity: &Identity) -> String {
    refresh_token(
        &jwt(),
        &Claims {
            sub: identity.display_name.clone(),
            role: identity.role,
            identity_id: Some(identity.id.to_string()),
            iat: 0,
            exp: 0,
            scopes: vec!["*".to_string()],
            device_id: None,
        },
    )
    .unwrap()
}

pub struct Gateway {
    pub state: Arc<GatewayState>,
    pub admin: Identity,
}

impl Gateway {
    /// A gateway serving `agents` (the first is the default), with revision
    /// history in `def_store` when one is given, and one signed-in admin.
    pub fn new(
        agents: Vec<(&str, Arc<AgentService>)>,
        def_store: Option<Arc<WorkflowStore>>,
    ) -> Self {
        let identities = SqliteIdentityStore::in_memory().expect("in-memory identity store");
        let admin = Identity::new("Admin", IdentityRole::SystemAdmin);
        identities.create(&admin).unwrap();
        let default_agent = agents
            .first()
            .map(|(id, _)| id.to_string())
            .unwrap_or_default();
        let registry = AgentRegistry::from_parts(
            agents
                .into_iter()
                .map(|(id, service)| (id.to_string(), service))
                .collect(),
            default_agent,
        );
        let state = GatewayParts {
            def_store,
            ..GatewayParts::new(
                registry,
                Arc::new(identities),
                std::path::PathBuf::from("/tmp/support-test-skills"),
            )
        }
        .state();
        Self { state, admin }
    }

    /// The admin's JWT, as a real sign-in mints it.
    pub fn admin_token(&self) -> String {
        token_for(&self.admin)
    }
}

/// Admin-API state with in-memory stores and an in-memory telemetry store,
/// on `data_dir`, and no LLM key.
pub fn hub_state(data_dir: &std::path::Path) -> workforce_hub::state::HubState {
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.hub.data_dir = data_dir.to_str().unwrap().to_string();
    config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
        .join("memory.db")
        .to_string_lossy()
        .into_owned();
    config.memory.backend = "in_memory".to_string();
    workforce_hub::state::HubState::for_tests(config)
}

/// An agent service on a fresh data directory, which must outlive it.
pub async fn agent_service(data_dir: &std::path::Path) -> Arc<AgentService> {
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.hub.data_dir = data_dir.to_string_lossy().to_string();
    config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
        .join("memory.db")
        .to_string_lossy()
        .into_owned();
    config.llm.api_key = Some("test-key".into());
    AgentService::from_config(config, None)
        .await
        .expect("test service")
}

/// An agent service on a fresh data directory whose model is the returned
/// mock: it streams `done` to every turn and records what it was asked.
pub async fn agent_on_mock_llm(
    data_dir: &std::path::Path,
) -> (Arc<AgentService>, wiremock::MockServer) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let chunk = |body: serde_json::Value| format!("data: {body}\n\n");
    let sse = [
        chunk(serde_json::json!({"id": "c1", "object": "chat.completion.chunk", "model": "mock",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "done"}, "finish_reason": null}]})),
        chunk(serde_json::json!({"id": "c1", "object": "chat.completion.chunk", "model": "mock",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.hub.data_dir = data_dir.to_string_lossy().to_string();
    config.memory.db_path = data_dir.join("memory.db").to_string_lossy().into_owned();
    config.llm.provider = "ollama".into();
    config.llm.api_key = Some(server.uri());
    config.llm.default_model = "mock".into();
    let service = AgentService::from_config(config, None)
        .await
        .expect("test service");
    (service, server)
}

/// Send `request` to `app`; the status, headers and body as text.
pub async fn send(
    app: axum::Router,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

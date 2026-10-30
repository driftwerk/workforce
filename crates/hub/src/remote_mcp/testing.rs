//! Fixtures shared by the remote MCP unit tests: a gateway state with real
//! identity and token stores and nothing else, plus token minting helpers.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use workforce_common::settings::RemoteMcpConfig;
use workforce_identity::remote_mcp_token::{self, RemoteMcpToken, RemoteMcpTokenStore};
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityAndTeamStore;
use workforce_identity::types::Identity;

use crate::agent_registry::AgentRegistry;
use crate::gateway::GatewayState;

pub(crate) struct Fixture {
    pub state: Arc<GatewayState>,
    pub identities: Arc<SqliteIdentityStore>,
    pub tokens: Arc<dyn RemoteMcpTokenStore>,
}

pub(crate) fn fixture(enabled: bool) -> Fixture {
    fixture_with(enabled, true)
}

/// `bind_tokens = false` leaves the gateway without a token store while the
/// fixture still holds one, for the fail-closed path.
pub(crate) fn fixture_with(enabled: bool, bind_tokens: bool) -> Fixture {
    let identities = Arc::new(SqliteIdentityStore::in_memory().expect("in-memory identity store"));
    let tokens = Arc::clone(&identities).as_remote_mcp_token_store();
    let (broadcast_tx, _rx) = tokio::sync::broadcast::channel(4);
    let skills_dir = std::path::PathBuf::from("/tmp/remote-mcp-test-skills");
    let external_skills = crate::external_skills::ExternalSkills::shared(&skills_dir);
    let state = Arc::new(GatewayState {
        registry: Arc::new(AgentRegistry::from_parts(
            HashMap::new(),
            "default".to_string(),
        )),
        nats: None,
        auth_token: "gateway-token".to_string(),
        broadcast_tx,
        progress_forwarders: Default::default(),
        channel_pairing: None,
        device_pairing: None,
        sentinel: Default::default(),
        access_gate: crate::access_gate::AccessGate::new(HashMap::new()),
        drain_token: tokio_util::sync::CancellationToken::new(),
        node_invoke_pending: Arc::default(),
        orchestrator: None,
        local_agents: Arc::new(crate::local_agent_registry::LocalAgentRegistry::new()),
        voice_listeners: Arc::new(crate::voice_announce::VoiceListeners::default()),
        vault: None,
        artifact_store: None,
        def_store: None,
        overlay_kick: None,
        workflow_runtime: None,
        playbook_runtime: None,
        consolidation_runtime: None,
        monitor_engine: None,
        engagement_overrides: Arc::new(workforce_common::engagement::EngagementOverrides::new()),
        config_path: None,
        identity_store: Some(
            Arc::clone(&identities) as Arc<dyn workforce_identity::store::IdentityStore>
        ),
        team_store: None,
        remote_mcp_tokens: bind_tokens.then(|| Arc::clone(&tokens)),
        remote_mcp: RemoteMcpConfig {
            enabled,
            ..Default::default()
        },
        permission_evaluator: None,
        destructive_baseline_active: false,
        telemetry_store: None,
        jwt_config: None,
        rate_limiter: None,
        pair_rate_limiter: None,
        action_limiter: None,
        trusted_proxies: Arc::new(
            crate::trusted_proxy::TrustedProxyConfig::from_strings(&[]).expect("empty proxies"),
        ),
        skills_dir,
        slack: Default::default(),
        skill_catalog: Arc::new(crate::skill_catalog::SkillCatalog::live(
            &Default::default(),
            None,
            Arc::clone(&external_skills),
        )),
        external_skills,
        #[cfg(feature = "oidc")]
        oidc_provider: None,
        pictures: None,
    });
    Fixture {
        state,
        identities,
        tokens,
    }
}

pub(crate) struct TokenShape {
    pub expires_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl Default for TokenShape {
    fn default() -> Self {
        Self {
            expires_at: Utc::now() + Duration::days(90),
            last_used_at: None,
            revoked_at: None,
        }
    }
}

/// Mint and persist a token for `identity`, returning the plaintext.
pub(crate) fn issue(fixture: &Fixture, identity: &Identity, shape: TokenShape) -> String {
    let minted = remote_mcp_token::mint();
    fixture
        .tokens
        .create_token(&RemoteMcpToken {
            id: uuid::Uuid::new_v4(),
            identity_id: identity.id,
            label: "test".to_string(),
            token_hash: minted.hash,
            token_prefix: minted.prefix,
            scopes: vec![super::tokens::TOKEN_SCOPE.to_string()],
            created_at: Utc::now(),
            expires_at: shape.expires_at,
            last_used_at: shape.last_used_at,
            revoked_at: shape.revoked_at,
        })
        .expect("token persists");
    minted.plaintext
}

pub(crate) fn bearer(token: &str) -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {token}").parse().expect("header value"),
    );
    headers
}

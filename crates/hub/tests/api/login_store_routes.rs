//! Every vault route that writes a subscription login goes through the
//! guarded store: a ChatGPT sign-in is stored in one place only, never
//! copied, and the hub's sign-ins in flight can't be written by a member.
//! A person's ChatGPT sign-in from the web is theirs alone, and only they,
//! signed in themselves, can change their logins.

use std::sync::Arc;
use workforce_hub::serve::Compose as _;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use base64::Engine;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use workforce_common::locking::LockManager;
use workforce_common::settings::{AccessMode, AgentAccessConfig, VaultActor};
use workforce_hub::agent_registry::AgentRegistry;
use workforce_hub::gateway::router;
use workforce_hub::jwt::generate_token_with_identity;
use workforce_hub::serve::AgentService;
use workforce_hub::subscription::{DeviceAuthClient, DeviceLogins, Subscriptions};
use workforce_hub::vault::Vault;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};
use workforce_pairing::{
    DevicePairingService, PairingConfig, SqlitePairingStore, DEFAULT_DEVICE_KIND,
};

use crate::support::{jwt, send, token_for, GatewayParts};

const CHATGPT: &str = "codex/auth_json";
const CODE: &str = "ABCD-1234";

fn jwt_token(claims: serde_json::Value) -> String {
    let encode = |value: &serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
    };
    format!(
        "{}.{}.c2lnbmF0dXJlLWJ5dGVz",
        encode(&serde_json::json!({"alg": "RS256", "typ": "JWT"})),
        encode(&claims)
    )
}

fn sign_in(refresh_token: &str) -> String {
    sign_in_as(refresh_token, serde_json::json!({"sub": "user-1"}))
}

/// A sign-in whose id token carries `claims`, on an account of its own.
fn sign_in_as(refresh_token: &str, claims: serde_json::Value) -> String {
    serde_json::json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": jwt_token(claims),
            "access_token": jwt_token(serde_json::json!({"exp": 4_102_444_800_i64})),
            "refresh_token": refresh_token,
            "account_id": format!("acct-{refresh_token}")
        }
    })
    .to_string()
}

struct Hub {
    state: Arc<workforce_hub::gateway::GatewayState>,
    vault: Arc<Vault>,
    agent: Arc<Vault>,
    member: Identity,
    other: Identity,
    admin: Identity,
    pairing: Arc<DevicePairingService>,
    _issuer: MockServer,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Hub {
    async fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let vaults = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(&vaults.path().join("global.enc"), "test-key").unwrap());
        let issuer = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/usercode"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_auth_id": "dev-secret-1",
                "user_code": CODE,
                "interval": "5",
            })))
            .mount(&issuer)
            .await;
        let identities = SqliteIdentityStore::in_memory().unwrap();
        let member = Identity::new("Alice", IdentityRole::Member);
        let other = Identity::new("Bob", IdentityRole::Member);
        let admin = Identity::new("Admin", IdentityRole::SystemAdmin);
        identities.create(&member).unwrap();
        identities.create(&other).unwrap();
        identities.create(&admin).unwrap();
        let mut config = workforce_common::settings::WorkforceConfig::default();
        config.hub.data_dir = data.path().to_string_lossy().to_string();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        config.llm.api_key = Some("test-key".into());
        let subscriptions =
            Subscriptions::boot(&config, Some(&vault)).with_device_logins(DeviceLogins::new(
                Some(Arc::clone(&vault)),
                DeviceAuthClient::at(reqwest::Client::new(), &issuer.uri()),
            ));
        let service = AgentService::build_for_agent(
            config,
            None,
            None,
            None,
            Arc::new(LockManager::new()),
            None,
            None,
            None,
            Some(Arc::clone(&vault)),
            subscriptions,
        )
        .await
        .unwrap();
        let agent = service.vault.clone().expect("agent vault");
        let registry = AgentRegistry::from_parts(
            [("scout".to_string(), service)].into_iter().collect(),
            "scout".into(),
        );
        let pairing = Arc::new(DevicePairingService::new(
            Arc::new(SqlitePairingStore::in_memory().unwrap()),
            PairingConfig {
                device_pairing_enabled: true,
                ..Default::default()
            },
        ));
        let state = GatewayParts {
            vault: Some(Arc::clone(&vault)),
            device_pairing: Some(Arc::clone(&pairing)),
            ..GatewayParts::new(
                registry,
                Arc::new(identities),
                std::path::PathBuf::from("/tmp/login-store-test-skills"),
            )
        }
        .state();
        Self {
            state,
            vault,
            agent,
            member,
            other,
            admin,
            pairing,
            _issuer: issuer,
            _dirs: (data, vaults),
        }
    }

    async fn call(
        &self,
        token: &str,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, String) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .unwrap();
        let (status, _, text) = send(router(Arc::clone(&self.state)), request).await;
        (status, text)
    }

    async fn post(
        &self,
        who: &Identity,
        path: &str,
        body: serde_json::Value,
    ) -> (StatusCode, String) {
        self.call(&token_for(who), Method::POST, path, Some(body))
            .await
    }

    async fn get(&self, who: &Identity, path: &str) -> (StatusCode, String) {
        self.call(&token_for(who), Method::GET, path, None).await
    }

    async fn delete(&self, who: &Identity, path: &str) -> (StatusCode, String) {
        self.call(&token_for(who), Method::DELETE, path, None).await
    }

    fn identities(&self) -> &Arc<dyn IdentityStore> {
        self.state.identity_store.as_ref().expect("identity store")
    }

    /// The token of a device paired to `who`, as a voice box or phone holds it.
    fn paired_token(&self, who: &Identity) -> String {
        let request = self
            .pairing
            .initiate("Voice box", vec!["*".into()], None, None, None, None)
            .unwrap();
        let (device, token) = self.pairing.approve(&request.code, "admin").unwrap();
        let linked = self
            .identities()
            .get_by_id(who.id)
            .unwrap()
            .unwrap()
            .with_linked_id(DEFAULT_DEVICE_KIND, device.id);
        self.identities().update(&linked).unwrap();
        token
    }

    /// The token of an admin demoted to a member while signed in: it is read
    /// as the member's, so another person's agent is not found.
    fn demoted_admin(&self) -> String {
        let mut dana = Identity::new("Dana", IdentityRole::SystemAdmin);
        self.identities().create(&dana).unwrap();
        let token = token_for(&dana);
        dana.set_role(IdentityRole::Member);
        self.identities().update(&dana).unwrap();
        token
    }

    fn scout(&self) -> Arc<AgentService> {
        self.state.registry.get("scout").expect("scout")
    }

    fn owned_by(&self, owner: &Identity) {
        *self.scout().owner_override.write() = Some((
            Some(owner.id.to_string()),
            workforce_common::settings::OwnerType::User,
        ));
    }

    /// Route the agent's failover through both subscriptions.
    fn route_subscriptions(&self, extra: &str) {
        std::fs::write(
            self.scout().workspace.join("config.toml"),
            format!("[llm]\nfailover = [\"claude-code:sonnet\", \"chatgpt:gpt-5.5\"]\n{extra}"),
        )
        .unwrap();
    }

    async fn logins_view(&self, who: &Identity) -> serde_json::Value {
        self.logins_view_as(&token_for(who)).await
    }

    async fn logins_status_as(&self, token: &str) -> StatusCode {
        self.call(token, Method::GET, "/api/agents/scout/logins", None)
            .await
            .0
    }

    async fn logins_view_as(&self, token: &str) -> serde_json::Value {
        let (status, text) = self
            .call(token, Method::GET, "/api/agents/scout/logins", None)
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        serde_json::from_str(&text).unwrap()
    }
}

const CLAUDE: &str = "claude_code/oauth_token";

fn claude_token(tag: &str) -> String {
    format!("sk-ant-oat01-{tag}-0123456789abcdefghijklmnopqrstuvwxyz")
}

#[tokio::test]
async fn generic_user_secret_write_of_duplicate_sign_in_is_refused() {
    let hub = Hub::new().await;
    let install = sign_in("rt-install");
    hub.vault.set(CHATGPT, &install).unwrap();
    let own = format!("users/{}/{CHATGPT}", hub.member.id);

    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/secret",
            serde_json::json!({"path": CHATGPT, "value": install, "tier": "global_per_user"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert!(text.contains("already stored"), "{text}");
    assert!(hub.vault.get(&own).is_none());

    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/secret",
            serde_json::json!({"path": CHATGPT, "value": "{\"tokens\": {}}", "tier": "global_per_user"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");

    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/secret",
            serde_json::json!({"path": CHATGPT, "value": sign_in("rt-alice"), "tier": "global_per_user"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(hub.vault.get(&own).is_some());
}

#[tokio::test]
async fn agent_vault_copy_of_chatgpt_login_is_refused() {
    let hub = Hub::new().await;
    let (status, text) = hub
        .post(
            &hub.admin,
            "/api/agents/scout/vault/copy",
            serde_json::json!({
                "path": CHATGPT,
                "from_tier": "global_per_user",
                "user_id": hub.admin.id.to_string(),
                "to_tier": "agent_per_user",
                "to_user_ids": [hub.member.id.to_string()],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert!(text.contains("can't be copied"), "{text}");
}

#[tokio::test]
async fn member_cannot_write_device_login_record() {
    let hub = Hub::new().await;
    let record = format!(
        "{}{}",
        workforce_hub::subscription::device_login::RECORD_PREFIX,
        hub.member.id
    );
    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/secret",
            serde_json::json!({"path": record, "value": "{}", "tier": "global_per_user"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert!(hub
        .vault
        .list(Some("users/"))
        .iter()
        .all(|key| !key.contains("device_login")));
    assert!(hub.vault.get(&record).is_none());
}

#[tokio::test]
async fn gateway_vault_routes_write_logins_through_the_guarded_store() {
    let hub = Hub::new().await;
    let shared = sign_in("rt-shared");
    let at = |route: &str| format!("{route}/{CHATGPT}");

    let (status, text) = hub
        .post(
            &hub.admin,
            &at("/api/vault"),
            serde_json::json!({"value": shared}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, text) = hub
        .post(
            &hub.admin,
            &at("/api/agents/scout/vault"),
            serde_json::json!({"value": shared}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert!(
        !text.contains(".enc") && !text.contains("postgres:"),
        "{text}"
    );
    let (status, text) = hub
        .post(
            &hub.admin,
            "/api/vault/rename",
            serde_json::json!({"from": CHATGPT, "to": "skills/x/token"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");

    let (status, text) = hub
        .post(
            &hub.admin,
            "/api/agents/scout/vault/move",
            serde_json::json!({"path": CHATGPT, "from_tier": "global", "to_tier": "agent_specific"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(hub.vault.get(CHATGPT).is_none());
    assert_eq!(hub.agent.get(CHATGPT).as_deref(), Some(shared.as_str()));
    let (status, text) = hub
        .post(
            &hub.admin,
            &at("/api/vault"),
            serde_json::json!({"value": shared}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");

    let (status, text) = hub.delete(&hub.admin, &at("/api/agents/scout/vault")).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, text) = hub
        .post(
            &hub.admin,
            &at("/api/vault"),
            serde_json::json!({"value": shared}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, text) = hub.delete(&hub.admin, &at("/api/vault")).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(hub.vault.get(CHATGPT).is_none() && hub.agent.get(CHATGPT).is_none());
}

#[tokio::test]
async fn my_settings_move_of_a_sign_in_leaves_one_copy() {
    let hub = Hub::new().await;
    let mine = sign_in("rt-alice");
    let own = format!("users/{}/{CHATGPT}", hub.member.id);
    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/secret",
            serde_json::json!({"path": CHATGPT, "value": mine, "tier": "global_per_user"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/vault/move",
            serde_json::json!({
                "path": CHATGPT,
                "from_tier": "global_per_user",
                "to_tier": "agent_per_user",
                "agent_id": "scout",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(hub.vault.get(&own).is_none());
    assert_eq!(hub.agent.get(&own).as_deref(), Some(mine.as_str()));

    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/secret",
            serde_json::json!({"path": CHATGPT, "value": mine, "tier": "global_per_user"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
}

#[tokio::test]
async fn device_sign_in_routes_serve_its_owner_alone() {
    let hub = Hub::new().await;
    let (status, started) = hub
        .post(
            &hub.member,
            "/api/me/subscriptions/chatgpt/device",
            serde_json::json!({"tier": "global_user"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let view: serde_json::Value = serde_json::from_str(&started).unwrap();
    assert_eq!(view["state"], "pending");
    assert_eq!(view["user_code"], CODE);
    assert!(
        !started.contains("dev-secret-1") && !started.contains("device_auth_id"),
        "{started}"
    );
    let flow = format!(
        "/api/me/subscriptions/chatgpt/device/{}",
        view["flow_id"].as_str().unwrap()
    );

    let (status, text) = hub.get(&hub.admin, &flow).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
    let (status, text) = hub.get(&hub.admin, "/api/vault").await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(!text.contains("device_login"), "{text}");
    let (status, text) = hub
        .get(
            &hub.admin,
            &format!(
                "/api/vault/{}{}",
                workforce_hub::subscription::device_login::RECORD_PREFIX,
                hub.member.id
            ),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
    let (status, text) = hub.delete(&hub.admin, &flow).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
    let (status, text) = hub
        .get(
            &hub.member,
            "/api/me/subscriptions/chatgpt/device?tier=global_user",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(text.contains(CODE), "{text}");

    let (status, text) = hub.delete(&hub.member, &flow).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, text) = hub.get(&hub.member, &flow).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let settled: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(settled["state"], "cancelled");
    assert!(
        !text.contains(CODE),
        "a settled flow carries no code: {text}"
    );

    let not_a_person = generate_token_with_identity(
        &jwt(),
        "Slack bridge",
        IdentityRole::Member,
        Some("slack-U123".into()),
    )
    .unwrap();
    let (status, text) = hub
        .call(
            &not_a_person,
            Method::POST,
            "/api/me/subscriptions/chatgpt/device",
            Some(serde_json::json!({"tier": "global_user"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
}

#[tokio::test]
async fn me_view_lists_global_and_agent_rows_without_tokens() {
    let hub = Hub::new().await;
    let token = claude_token("alice");
    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/subscriptions/claude/login",
            serde_json::json!({"tier": "global_user", "value": token}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/subscriptions/chatgpt/login",
            serde_json::json!({"tier": "agent_user", "agent_id": "scout", "value": sign_in("rt-alice")}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(hub
        .agent
        .get(&format!("users/{}/{CHATGPT}", hub.member.id))
        .is_some());

    let (status, text) = hub.get(&hub.member, "/api/me/subscriptions").await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(
        !text.contains(&token) && !text.contains("rt-alice"),
        "{text}"
    );
    let view: serde_json::Value = serde_json::from_str(&text).unwrap();
    let rows: Vec<(String, String, Option<String>, String)> = view["logins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["kind"].as_str().unwrap().to_string(),
                row["tier"].as_str().unwrap().to_string(),
                row["agent_id"].as_str().map(str::to_string),
                row["status"]["state"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let row = |kind: &str, tier: &str, agent: Option<&str>, state: &str| {
        (
            kind.to_string(),
            tier.to_string(),
            agent.map(str::to_string),
            state.to_string(),
        )
    };
    assert_eq!(
        rows,
        [
            row("claude", "global_user", None, "connected"),
            row("chatgpt", "global_user", None, "not_set"),
            row("chatgpt", "agent_user", Some("scout"), "connected"),
        ]
    );
    assert_eq!(view["logins"][0]["updated_by"], "Alice");

    let (status, text) = hub
        .delete(
            &hub.member,
            "/api/me/subscriptions/chatgpt/login?tier=agent_user&agent_id=scout",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let removed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(removed["removed"], true);
    let (_, text) = hub.get(&hub.member, "/api/me/subscriptions").await;
    assert!(!text.contains("\"agent_user\""), "{text}");
}

#[tokio::test]
async fn paste_refuses_api_key_auth_json() {
    let hub = Hub::new().await;
    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/subscriptions/chatgpt/login",
            serde_json::json!({
                "tier": "global_user",
                "value": r#"{"OPENAI_API_KEY":"sk-proj-0123456789abcdefghijklmnop"}"#,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(text.contains("holds an API key"), "{text}");
    assert!(hub
        .vault
        .get(&format!("users/{}/{CHATGPT}", hub.member.id))
        .is_none());
}

#[tokio::test]
async fn agent_shared_write_requires_owner_or_vault_shared_write() {
    let hub = Hub::new().await;
    let shared = serde_json::json!({"tier": "agent_shared", "agent_id": "scout", "value": claude_token("shared")});
    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/subscriptions/claude/login",
            shared.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/subscriptions/chatgpt/device",
            serde_json::json!({"tier": "agent_shared", "agent_id": "scout"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert!(hub.agent.get(CLAUDE).is_none());

    hub.owned_by(&hub.member);
    let (status, text) = hub
        .post(&hub.member, "/api/me/subscriptions/claude/login", shared)
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        hub.agent.get(CLAUDE).as_deref(),
        Some(claude_token("shared").as_str())
    );
    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/subscriptions/chatgpt/device",
            serde_json::json!({"tier": "agent_shared", "agent_id": "scout"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let remove = "/api/me/subscriptions/claude/login?tier=agent_shared&agent_id=scout";
    let (status, text) = hub.delete(&hub.other, remove).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    let (status, text) = hub.delete(&hub.admin, remove).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(hub.agent.get(CLAUDE).is_none());
}

#[tokio::test]
async fn system_shared_write_requires_admin() {
    let hub = Hub::new().await;
    hub.route_subscriptions("");
    let install = serde_json::json!({"tier": "system_shared", "value": claude_token("install")});
    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/subscriptions/claude/login",
            install.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert!(hub.vault.get(CLAUDE).is_none());
    hub.owned_by(&hub.member);
    let view = hub.logins_view(&hub.member).await;
    assert_eq!(view["can_manage_shared"], true);
    assert_eq!(view["can_manage_install"], false);

    let (status, text) = hub
        .post(&hub.admin, "/api/me/subscriptions/claude/login", install)
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        hub.vault.get(CLAUDE).as_deref(),
        Some(claude_token("install").as_str())
    );
    let view = hub.logins_view(&hub.admin).await;
    assert_eq!(view["can_manage_install"], true);
    let install_row = view["shared"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["kind"] == "claude" && row["tier"] == "system_shared")
        .cloned()
        .unwrap();
    assert_eq!(install_row["status"]["state"], "connected");
    assert!(!view.to_string().contains(&claude_token("install")));
}

#[tokio::test]
async fn users_hidden_from_non_managers() {
    let hub = Hub::new().await;
    hub.route_subscriptions("");
    let (status, text) = hub
        .post(
            &hub.other,
            "/api/me/subscriptions/claude/login",
            serde_json::json!({"tier": "agent_user", "agent_id": "scout", "value": claude_token("bob")}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    hub.owned_by(&hub.member);

    assert_eq!(
        hub.logins_status_as(&hub.demoted_admin()).await,
        StatusCode::NOT_FOUND,
        "a demoted admin's token reads no agent of another person's"
    );

    let view = hub.logins_view(&hub.member).await;
    assert_eq!(view["can_manage_shared"], true);
    assert_eq!(
        view["users"],
        serde_json::json!([{"kind": "claude", "tier": "agent_user", "display_name": "Bob"}])
    );
}

#[tokio::test]
async fn users_follow_the_agent_login_tiers() {
    let hub = Hub::new().await;
    for (tier, value) in [
        ("agent_user", claude_token("bob-agent")),
        ("global_user", claude_token("bob-global")),
    ] {
        let (status, text) = hub
            .post(
                &hub.other,
                "/api/me/subscriptions/claude/login",
                serde_json::json!({"tier": tier, "agent_id": "scout", "value": value}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }
    hub.owned_by(&hub.member);
    hub.route_subscriptions(
        "[llm.logins]\ntiers = [\"global_user\", \"agent_shared\"]\nshared_serves = \"anyone\"\n",
    );

    let view = hub.logins_view(&hub.member).await;
    assert_eq!(
        view["users"],
        serde_json::json!([{"kind": "claude", "tier": "global_user", "display_name": "Bob"}])
    );
}

#[tokio::test]
async fn shared_login_account_hidden_from_non_managers() {
    let hub = Hub::new().await;
    hub.route_subscriptions("");
    let account = |refresh: &str, email: &str| {
        sign_in_as(
            refresh,
            serde_json::json!({
                "sub": refresh,
                "email": email,
                "https://api.openai.com/auth": {"chatgpt_plan_type": "plus"},
            }),
        )
    };
    let admin = hub.admin.id.to_string();
    hub.agent
        .set_with_actor(
            CHATGPT,
            &account("rt-agent", "owner@example.com"),
            VaultActor::User(&admin),
        )
        .unwrap();
    hub.vault
        .set_with_actor(
            CHATGPT,
            &account("rt-install", "install@example.com"),
            VaultActor::User(&admin),
        )
        .unwrap();
    hub.owned_by(&hub.member);
    let row = |view: &serde_json::Value, tier: &str| {
        view["shared"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["kind"] == "chatgpt" && row["tier"] == tier)
            .cloned()
            .unwrap()
    };
    let shown = |row: &serde_json::Value| {
        (
            row["status"]["state"].clone(),
            row["status"]["account"]["email"].clone(),
            row["updated_by"].clone(),
        )
    };
    let connected = serde_json::json!("connected");
    let hidden = (
        connected.clone(),
        serde_json::Value::Null,
        serde_json::Value::Null,
    );

    let admin_view = hub.logins_view(&hub.admin).await;
    assert_eq!(
        shown(&row(&admin_view, "agent_shared")),
        (
            connected.clone(),
            serde_json::json!("owner@example.com"),
            serde_json::json!("Admin")
        )
    );
    assert_eq!(
        shown(&row(&admin_view, "system_shared")),
        (
            connected.clone(),
            serde_json::json!("install@example.com"),
            serde_json::json!("Admin")
        )
    );

    let owner_view = hub.logins_view(&hub.member).await;
    assert_eq!(
        shown(&row(&owner_view, "agent_shared")).1,
        "owner@example.com"
    );
    assert_eq!(shown(&row(&owner_view, "system_shared")), hidden);

    assert_eq!(
        hub.logins_status_as(&hub.demoted_admin()).await,
        StatusCode::NOT_FOUND,
        "a demoted admin's token reads no agent of another person's"
    );
}

#[tokio::test]
async fn paired_device_token_cannot_write_or_read_logins() {
    let hub = Hub::new().await;
    let device = hub.paired_token(&hub.member);
    let (status, text) = hub.call(&device, Method::GET, "/api/me", None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the device is paired to Alice: {text}"
    );
    assert!(text.contains(&hub.member.id.to_string()), "{text}");

    let login = serde_json::json!({"tier": "global_user", "value": sign_in("rt-device")});
    for (method, path, body) in [
        (
            Method::POST,
            "/api/me/subscriptions/chatgpt/login",
            Some(login),
        ),
        (
            Method::DELETE,
            "/api/me/subscriptions/chatgpt/login?tier=global_user",
            None,
        ),
        (Method::GET, "/api/me/subscriptions", None),
        (
            Method::POST,
            "/api/me/subscriptions/chatgpt/device",
            Some(serde_json::json!({"tier": "global_user"})),
        ),
        (Method::GET, "/api/me/subscriptions/chatgpt/device", None),
    ] {
        let (status, text) = hub.call(&device, method.clone(), path, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}: {text}");
    }
    assert!(hub
        .vault
        .get(&format!("users/{}/{CHATGPT}", hub.member.id))
        .is_none());
}

#[tokio::test]
async fn member_cannot_store_login_on_agent_they_cannot_read() {
    let hub = Hub::new().await;
    hub.state.access_gate.upsert(
        "scout".into(),
        AgentAccessConfig {
            mode: AccessMode::Allowlist,
            allow: vec![hub.member.id.to_string()],
            ..AgentAccessConfig::default()
        },
    );
    let bobs = format!("users/{}/{CLAUDE}", hub.other.id);
    hub.agent.set(&bobs, &claude_token("bob-earlier")).unwrap();
    let on_scout = serde_json::json!({"tier": "agent_user", "agent_id": "scout", "value": claude_token("bob")});

    let (status, text) = hub
        .post(&hub.other, "/api/me/subscriptions/claude/login", on_scout)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
    assert_eq!(text, r#"{"error":"agent 'scout' not found"}"#);
    let (status, text) = hub
        .post(
            &hub.other,
            "/api/me/subscriptions/chatgpt/device",
            serde_json::json!({"tier": "agent_user", "agent_id": "scout"}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
    let (status, text) = hub
        .delete(
            &hub.other,
            "/api/me/subscriptions/claude/login?tier=agent_user&agent_id=scout",
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
    assert_eq!(
        hub.agent.get(&bobs).as_deref(),
        Some(claude_token("bob-earlier").as_str())
    );
    let (status, text) = hub.get(&hub.other, "/api/me/subscriptions").await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(!text.contains("scout"), "{text}");

    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/subscriptions/claude/login",
            serde_json::json!({"tier": "agent_user", "agent_id": "scout", "value": claude_token("alice")}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
}

#[tokio::test]
async fn current_device_login_without_a_target_is_the_one_kept() {
    let hub = Hub::new().await;
    let (status, text) = hub
        .get(&hub.member, "/api/me/subscriptions/chatgpt/device")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
    let (status, started) = hub
        .post(
            &hub.member,
            "/api/me/subscriptions/chatgpt/device",
            serde_json::json!({"tier": "agent_user", "agent_id": "scout"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let started: serde_json::Value = serde_json::from_str(&started).unwrap();

    let (status, text) = hub
        .get(&hub.member, "/api/me/subscriptions/chatgpt/device")
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let current: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(current["flow_id"], started["flow_id"]);
    assert_eq!(current["state"], "pending");
    assert_eq!(
        current["target"],
        serde_json::json!({"tier": "agent_user", "agent_id": "scout", "agent_name": "scout"})
    );
    let (status, text) = hub
        .get(
            &hub.member,
            "/api/me/subscriptions/chatgpt/device?tier=global_user",
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
    let (status, text) = hub
        .get(&hub.other, "/api/me/subscriptions/chatgpt/device")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
}

#[tokio::test]
async fn serving_you_names_the_tier_resolve_picks() {
    let hub = Hub::new().await;
    hub.route_subscriptions("");
    hub.owned_by(&hub.member);
    let serving = |view: &serde_json::Value, kind: &str| {
        view["serving_you"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["kind"] == kind)
            .cloned()
            .unwrap()
    };

    let view = hub.logins_view(&hub.member).await;
    assert_eq!(view["kinds"], serde_json::json!(["claude", "chatgpt"]));
    assert_eq!(view["policy"]["overridden"], false);
    assert!(view["route_options"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("claude-code:sonnet")));
    let claude = serving(&view, "claude");
    assert!(claude["tier"].is_null());
    assert_eq!(
        claude["misses"][0],
        serde_json::json!({"tier": "agent_user", "reason": "none stored"})
    );

    hub.vault.set(CLAUDE, &claude_token("install")).unwrap();
    let view = hub.logins_view(&hub.member).await;
    assert_eq!(serving(&view, "claude")["tier"], "system_shared");

    let (status, text) = hub
        .post(
            &hub.member,
            "/api/me/subscriptions/claude/login",
            serde_json::json!({"tier": "global_user", "value": claude_token("alice")}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        serving(&hub.logins_view(&hub.member).await, "claude")["tier"],
        "global_user"
    );

    hub.route_subscriptions(
        "[llm.logins]\ntiers = [\"agent_shared\", \"system_shared\"]\nshared_serves = \"owner\"\n",
    );
    let view = hub.logins_view(&hub.member).await;
    assert_eq!(view["policy"]["overridden"], true);
    assert_eq!(serving(&view, "claude")["tier"], "system_shared");
    let view = hub.logins_view(&hub.admin).await;
    let claude = serving(&view, "claude");
    assert!(claude["tier"].is_null());
    assert_eq!(
        claude["misses"],
        serde_json::json!([
            {"tier": "agent_shared", "reason": "shared login serves Owner only"},
            {"tier": "system_shared", "reason": "shared login serves Owner only"},
        ])
    );
}

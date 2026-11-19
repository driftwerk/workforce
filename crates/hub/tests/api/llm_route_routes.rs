//! A person's own route from the web: their every-agent and per-agent
//! settings, the narrower winning, refused while the hub's switch is off,
//! for a kind not offered or not routable yet, and without a working login
//! of their own. Each person sees and changes only their own, signed in
//! themselves, and every change is kept as an audit row.

use std::sync::Arc;
use workforce_hub::serve::Compose as _;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use serde_json::{json, Value};
use workforce_common::locking::LockManager;
use workforce_common::personal_route::{
    AgentPersonalRoutes, PersonalRouteKind, PersonalRoutesConfig,
};
use workforce_hub::agent_registry::AgentRegistry;
use workforce_hub::gateway::router;
use workforce_hub::hub_core::personal_routes::{MemoryRouteStore, PersonRouteStore};
use workforce_hub::serve::AgentService;
use workforce_hub::subscription::Subscriptions;
use workforce_hub::vault::Vault;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};

use crate::support::{send, token_for, GatewayParts};

const CLAUDE_LOGIN: &str = "claude_code/oauth_token";

struct Hub {
    state: Arc<workforce_hub::gateway::GatewayState>,
    vault: Arc<Vault>,
    store: Arc<MemoryRouteStore>,
    service: Arc<AgentService>,
    alice: Identity,
    bob: Identity,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Hub {
    async fn new(switch: PersonalRoutesConfig) -> Self {
        let data = tempfile::tempdir().unwrap();
        let vaults = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(&vaults.path().join("global.enc"), "test-key").unwrap());
        let identities = SqliteIdentityStore::in_memory().unwrap();
        let alice = Identity::new("Alice", IdentityRole::Member);
        let bob = Identity::new("Bob", IdentityRole::Member);
        identities.create(&alice).unwrap();
        identities.create(&bob).unwrap();
        let mut config = workforce_common::settings::WorkforceConfig::default();
        config.hub.data_dir = data.path().to_string_lossy().to_string();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        config.llm.api_key = Some("test-key".into());
        config.llm.personal_routes = switch;
        let subscriptions = Subscriptions::boot(&config, Some(&vault));
        let store = Arc::new(MemoryRouteStore::default());
        subscriptions
            .shared()
            .personal_routes()
            .attach_store(Arc::clone(&store) as Arc<dyn PersonRouteStore>);
        // Built as the agent it's listed as, so a turn's settings and the
        // routes' are keyed alike.
        let scout = workforce_common::settings::AgentConfig {
            id: "scout".into(),
            ..Default::default()
        };
        let service = AgentService::build_for_agent(
            config,
            None,
            Some(&scout),
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
        let registry = AgentRegistry::from_parts(
            [("scout".to_string(), Arc::clone(&service))]
                .into_iter()
                .collect(),
            "scout".into(),
        );
        let state = GatewayParts {
            vault: Some(Arc::clone(&vault)),
            ..GatewayParts::new(
                registry,
                Arc::new(identities),
                std::path::PathBuf::from("/tmp/llm-route-test-skills"),
            )
        }
        .state();
        Self {
            state,
            vault,
            store,
            service,
            alice,
            bob,
            _dirs: (data, vaults),
        }
    }

    async fn on() -> Self {
        Self::new(PersonalRoutesConfig {
            enabled: true,
            ..Default::default()
        })
        .await
    }

    async fn call(
        &self,
        who: &Identity,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {}", token_for(who)))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .unwrap();
        let (status, _, text) = send(router(Arc::clone(&self.state)), request).await;
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn put(&self, who: &Identity, path: &str, route: Value) -> (StatusCode, Value) {
        self.call(who, Method::PUT, path, Some(json!({ "route": route })))
            .await
    }

    async fn get(&self, who: &Identity, path: &str) -> (StatusCode, Value) {
        self.call(who, Method::GET, path, None).await
    }

    /// `who`'s own Claude login for the whole hub.
    fn claude_login(&self, who: &Identity) {
        self.vault
            .set(
                &format!("users/{}/{CLAUDE_LOGIN}", who.id),
                &format!("token-{}", uuid::Uuid::new_v4()),
            )
            .unwrap();
    }
}

impl Hub {
    /// `who`'s own ChatGPT login for the whole hub.
    fn chatgpt_login(&self, who: &Identity) {
        self.vault
            .set(
                &format!("users/{}/codex/auth_json", who.id),
                &workforce_hub_core::hub_core::logins::test_support::login(
                    &format!("acct-{}", uuid::Uuid::new_v4()),
                    "alice",
                    "rt-0",
                ),
            )
            .unwrap();
    }

    /// The agent's own `[llm.personal_routes]`, as its config now says.
    fn agent_routes(&self, own: AgentPersonalRoutes) {
        let mut resolved = (*self.service.resolved.snapshot()).clone();
        resolved.personal_routes = Some(own);
        self.service.resolved.store(resolved);
    }
}

/// Why `kind` can't be picked, as `view`'s options say; `None` when it can.
fn refused<'a>(view: &'a Value, kind: &str) -> Option<&'a str> {
    let option = view["options"]
        .as_array()
        .unwrap_or_else(|| panic!("options listed: {view}"))
        .iter()
        .find(|option| option["kind"] == kind)
        .unwrap_or_else(|| panic!("{kind} listed: {view}"));
    option["refused"].as_str()
}

fn claude(fallback: &str) -> Value {
    json!({ "kind": "claude-code", "fallback": fallback })
}

#[tokio::test]
async fn a_person_sets_their_every_agent_route_with_a_login_of_their_own() {
    let hub = Hub::on().await;
    let (status, body) = hub.get(&hub.alice, "/api/me/llm-route").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["available"], true);
    assert_eq!(body["route"], Value::Null);
    assert!(
        refused(&body, "claude-code").is_some_and(|why| why.contains("Claude login")),
        "greyed out until she connects her own: {body}"
    );
    assert!(refused(&body, "devin").is_some(), "{body}");

    let (status, body) = hub
        .put(&hub.alice, "/api/me/llm-route", claude("wait"))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "no login of her own yet: {body}"
    );
    assert!(body["error"].as_str().unwrap().contains("Claude login"));

    hub.claude_login(&hub.alice);
    let (status, body) = hub
        .put(&hub.alice, "/api/me/llm-route", claude("hub"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["route"], claude("hub"));
    assert_eq!(refused(&body, "claude-code"), None, "{body}");
    let (_, body) = hub.get(&hub.alice, "/api/me/llm-route").await;
    assert_eq!(body["route"], claude("hub"));
    assert_eq!(refused(&body, "claude-code"), None, "offered now: {body}");

    let (_, theirs) = hub.get(&hub.bob, "/api/me/llm-route").await;
    assert_eq!(theirs["route"], Value::Null, "Bob's setting is his own");

    let (status, body) = hub.put(&hub.alice, "/api/me/llm-route", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["route"], Value::Null);

    let changes = hub.store.changes(hub.alice.id, 10).await.unwrap();
    assert_eq!(changes.len(), 2, "each change is an audit row");
    assert!(changes.iter().all(|c| c.actor == hub.alice.id.to_string()));
}

#[tokio::test]
async fn the_per_agent_setting_wins_and_hub_refuses_the_every_agent_one() {
    let hub = Hub::on().await;
    hub.claude_login(&hub.alice);
    hub.put(&hub.alice, "/api/me/llm-route", claude("wait"))
        .await;

    let path = "/api/me/agents/scout/llm-route";
    let (status, body) = hub.get(&hub.alice, path).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["route"], Value::Null);
    assert_eq!(body["effective"], claude("wait"), "as in her settings");
    assert_eq!(refused(&body, "claude-code"), None, "{body}");

    let (status, body) = hub.put(&hub.alice, path, json!("hub")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["route"], "hub");
    assert_eq!(body["every_agent"], claude("wait"));
    assert_eq!(
        body["effective"],
        Value::Null,
        "this agent stays on the hub's route"
    );

    let (_, body) = hub.put(&hub.alice, path, claude("hub")).await;
    assert_eq!(
        body["effective"],
        claude("hub"),
        "the narrower setting wins"
    );

    let (_, body) = hub.put(&hub.alice, path, Value::Null).await;
    assert_eq!(body["route"], Value::Null);
    assert_eq!(body["effective"], claude("wait"));

    let (status, _) = hub.get(&hub.alice, "/api/me/agents/nobody/llm-route").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_route_is_refused_while_the_switch_is_off_for_unroutable_kinds_and_without_a_login() {
    let off = Hub::new(PersonalRoutesConfig::default()).await;
    off.claude_login(&off.alice);
    let (_, body) = off.get(&off.alice, "/api/me/llm-route").await;
    assert_eq!(body["available"], false);
    let (status, body) = off
        .put(&off.alice, "/api/me/llm-route", claude("wait"))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _) = off.put(&off.alice, "/api/me/llm-route", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "clearing is always allowed");

    let on = Hub::on().await;
    on.claude_login(&on.alice);
    for (kind, refused) in [
        ("codex", "connect your own ChatGPT (Codex) login"),
        ("devin", "isn't available yet"),
    ] {
        let (status, body) = on
            .put(
                &on.alice,
                "/api/me/llm-route",
                json!({ "kind": kind, "fallback": "wait" }),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{kind}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains(refused),
            "{kind}: {body}"
        );
    }
    let (status, _) = on
        .put(&on.alice, "/api/me/llm-route", json!({ "kind": "gemini" }))
        .await;
    assert!(status.is_client_error(), "an unknown kind is refused");
}

#[tokio::test]
async fn a_codex_route_is_kept_for_every_agent_and_refused_on_an_agent_without_a_coding_worker() {
    let hub = Hub::on().await;
    hub.chatgpt_login(&hub.alice);
    let codex = json!({ "kind": "codex", "fallback": "wait" });

    let (status, body) = hub
        .put(&hub.alice, "/api/me/llm-route", codex.clone())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["route"], codex);

    let (status, body) = hub
        .put(&hub.alice, "/api/me/agents/scout/llm-route", codex)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["error"].as_str(),
        Some(workforce_hub::personal_route::CODEX_NEEDS_WORKER),
        "an agent without the hub's coding worker says why"
    );
    let devin = json!({ "kind": "devin", "fallback": "wait" });
    let (status, body) = hub.put(&hub.alice, "/api/me/llm-route", devin).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("isn't available yet"));
}

#[tokio::test]
async fn an_agent_that_opted_out_or_narrowed_the_kinds_refuses_a_route_of_its_own() {
    let hub = Hub::on().await;
    hub.claude_login(&hub.alice);
    let path = "/api/me/agents/scout/llm-route";
    hub.agent_routes(AgentPersonalRoutes {
        enabled: Some(false),
        allow: None,
    });
    let (_, body) = hub.get(&hub.alice, path).await;
    assert_eq!(body["available"], false, "{body}");
    assert!(
        refused(&body, "claude-code").is_some_and(|why| why.contains("this agent")),
        "greyed out with the agent's reason: {body}"
    );
    let (status, body) = hub.put(&hub.alice, path, claude("wait")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("this agent"),
        "{body}"
    );
    let (status, _) = hub.put(&hub.alice, path, json!("hub")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "keeping it on the hub's route is always allowed"
    );

    hub.agent_routes(AgentPersonalRoutes {
        enabled: None,
        allow: Some(vec![PersonalRouteKind::Codex]),
    });
    let (status, body) = hub.put(&hub.alice, path, claude("wait")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("isn't offered here"),
        "{body}"
    );
    let (status, _) = hub
        .put(&hub.alice, "/api/me/llm-route", claude("wait"))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the every-agent setting follows the hub's switch"
    );

    let (status, _) = hub
        .call(&hub.alice, Method::PUT, path, Some(json!({})))
        .await;
    assert!(
        status.is_client_error(),
        "an empty body isn't read as a clear"
    );
}

#[tokio::test]
async fn her_routes_models_are_kept_and_a_retired_one_is_refused_in_plain_words() {
    let hub = Hub::on().await;
    hub.chatgpt_login(&hub.alice);

    let (_, body) = hub.get(&hub.alice, "/api/me/llm-route").await;
    let codex = body["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["kind"] == "codex")
        .unwrap()
        .clone();
    let models: Vec<&str> = codex["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m.as_str())
        .collect();
    assert!(
        models.contains(&"gpt-6.1-sol"),
        "the plan's models: {codex}"
    );
    assert!(!models.contains(&"gpt-5.5"), "never a retired one");
    assert_eq!(codex["defaults"]["conversation"], "gpt-6.1-sol");

    let chosen = json!({
        "kind": "codex",
        "fallback": "wait",
        "model_preferences": { "conversation": "gpt-6-astra" },
        "light_model": "cheap",
        "heavy_model": "chat",
    });
    let (status, body) = hub
        .put(&hub.alice, "/api/me/llm-route", chosen.clone())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["route"], chosen);
    let (_, body) = hub.get(&hub.alice, "/api/me/llm-route").await;
    assert_eq!(body["route"], chosen, "kept");

    let retired = json!({ "kind": "codex", "fallback": "wait", "heavy_model": "gpt-5.5" });
    let (status, body) = hub.put(&hub.alice, "/api/me/llm-route", retired).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let why = body["error"].as_str().unwrap();
    assert!(
        why.contains("heavy") && why.contains("retired") && why.contains("gpt-6.1-sol"),
        "{why}"
    );
    let (_, body) = hub.get(&hub.alice, "/api/me/llm-route").await;
    assert_eq!(body["route"], chosen, "a refused change keeps what she had");

    let unknown = json!({ "kind": "codex", "fallback": "wait", "model_preferences": { "coding": "gpt-6-sol" } });
    let (status, _) = hub.put(&hub.alice, "/api/me/llm-route", unknown).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// The agent `scout` as `/api/agents` lists it.
fn scout(listing: &Value) -> &Value {
    listing["agents"]
        .as_array()
        .unwrap_or_else(|| panic!("agents listed: {listing}"))
        .iter()
        .find(|agent| agent["id"] == "scout")
        .unwrap_or_else(|| panic!("scout listed: {listing}"))
}

#[tokio::test]
async fn the_agent_list_says_what_each_persons_own_conversations_run_on() {
    let hub = Hub::on().await;
    let (status, before) = hub.get(&hub.alice, "/api/agents").await;
    assert_eq!(status, StatusCode::OK, "{before}");
    assert!(
        scout(&before).get("personal_route").is_none(),
        "the hub's route: {before}"
    );

    hub.claude_login(&hub.alice);
    hub.put(&hub.alice, "/api/me/llm-route", claude("wait"))
        .await;
    let (_, listing) = hub.get(&hub.alice, "/api/agents").await;
    let agent = scout(&listing);
    let route = &agent["personal_route"];
    assert_eq!(route["kind"], "claude-code", "{listing}");
    assert_eq!(route["title"], "Claude");
    assert_eq!(route["fallback"], "wait");
    assert_eq!(route["connected"], true);
    assert!(
        route["model"].as_str().is_some_and(|m| !m.is_empty()),
        "{listing}"
    );
    assert_eq!(
        (&agent["provider"], &agent["model"]),
        (&scout(&before)["provider"], &scout(&before)["model"]),
        "the hub's own route is still said as it was"
    );

    let (_, theirs) = hub.get(&hub.bob, "/api/agents").await;
    assert!(
        scout(&theirs).get("personal_route").is_none(),
        "Bob's turns stay on the hub's route: {theirs}"
    );

    let path = "/api/me/agents/scout/llm-route";
    let (status, body) = hub.put(&hub.alice, path, json!("hub")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, listing) = hub.get(&hub.alice, "/api/agents").await;
    assert!(
        scout(&listing).get("personal_route").is_none(),
        "this agent stays on the hub's route: {listing}"
    );

    hub.put(&hub.alice, path, claude("hub")).await;
    let (_, listing) = hub.get(&hub.alice, "/api/agents").await;
    assert_eq!(scout(&listing)["personal_route"]["fallback"], "hub");

    hub.agent_routes(AgentPersonalRoutes {
        enabled: Some(false),
        allow: None,
    });
    let (_, listing) = hub.get(&hub.alice, "/api/agents").await;
    assert!(
        scout(&listing).get("personal_route").is_none(),
        "the agent opted out: {listing}"
    );
}

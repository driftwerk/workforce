//! Connecting a device to an account: the person signs in on `/pair` and
//! approves the code into their own account; an admin can assign a waiting
//! device to someone; the older admin route still links by the device's hint.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use workforce_hub::agent_registry::AgentRegistry;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};
use workforce_pairing::types::WhoCanConnect;
use workforce_pairing::{DevicePairingService, PairingConfig, SqlitePairingStore};

use crate::support::{agent_service, serve, token_for, GatewayParts, GATEWAY_TOKEN};

struct Hub {
    addr: SocketAddr,
    identities: Arc<dyn IdentityStore>,
    _data: tempfile::TempDir,
}

async fn hub_with(config: PairingConfig) -> Hub {
    let data = tempfile::tempdir().unwrap();
    let service = agent_service(data.path()).await;
    let identities: Arc<dyn IdentityStore> = Arc::new(SqliteIdentityStore::in_memory().unwrap());
    let state = GatewayParts {
        device_pairing: Some(Arc::new(DevicePairingService::new(
            Arc::new(SqlitePairingStore::in_memory().unwrap()),
            PairingConfig {
                device_pairing_enabled: true,
                ..config
            },
        ))),
        ..GatewayParts::new(
            AgentRegistry::from_parts(
                [("default".to_string(), service)].into_iter().collect(),
                "default".into(),
            ),
            Arc::clone(&identities),
            std::path::PathBuf::from("/tmp/pair-routes-test-skills"),
        )
    }
    .state();
    Hub {
        addr: serve(state).await,
        identities,
        _data: data,
    }
}

async fn hub() -> Hub {
    hub_with(PairingConfig::default()).await
}

impl Hub {
    fn person(&self, email: &str, role: IdentityRole) -> (Identity, String) {
        let identity = Identity::new(email, role).with_email(email);
        self.identities.create(&identity).unwrap();
        let token = token_for(&identity);
        (identity, token)
    }

    async fn call(
        &self,
        bearer: Option<&str>,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request =
            reqwest::Client::new().request(method, format!("http://{}{path}", self.addr));
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    /// A device asks for a code, the way Desk and the iPhone do.
    async fn request_code(&self, hint: Option<&str>) -> Value {
        let (status, body) = self
            .call(
                None,
                Method::POST,
                "/api/pair/device/request",
                Some(json!({
                    "device_name": "Studio Mac",
                    "device_kind": "desk",
                    "identity_hint": hint,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn approve_self(&self, bearer: &str, code: &str) -> (StatusCode, Value) {
        self.call(
            Some(bearer),
            Method::POST,
            "/api/pair/device/approve-self",
            Some(json!({ "code": code })),
        )
        .await
    }

    async fn status(&self, code: &str) -> (StatusCode, Value) {
        self.call(
            None,
            Method::GET,
            &format!("/api/pair/device/status/{code}"),
            None,
        )
        .await
    }

    fn device_owner(&self, person: &Identity) -> Vec<String> {
        self.identities
            .get_by_id(person.id)
            .unwrap()
            .unwrap()
            .linked_ids
            .into_keys()
            .collect()
    }
}

#[tokio::test]
async fn the_signed_in_person_gets_the_device_whoever_it_claims_to_be() {
    let hub = hub().await;
    let (kings, kings_token) = hub.person("kings@example.com", IdentityRole::Member);
    let (other, _) = hub.person("other@example.com", IdentityRole::Member);
    let asked = hub.request_code(Some("other@example.com")).await;
    let code = asked["code"].as_str().unwrap();
    assert!(
        asked["approve_url"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/pair?code={code}")),
        "{asked}"
    );

    // What the page shows: the device, never who asked.
    let (status, shown) = hub
        .call(
            Some(&kings_token),
            Method::GET,
            &format!("/api/pair/device/pending/{code}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(shown["device_name"], "Studio Mac");
    assert_eq!(
        shown["device_kind"], "desktop_device",
        "desk is a desktop device"
    );
    assert!(!shown.to_string().contains("other@example.com"), "{shown}");

    let (status, approved) = hub.approve_self(&kings_token, code).await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["identity"]["email"], "kings@example.com");
    assert!(approved.get("token").is_none());

    let (_, polled) = hub.status(code).await;
    assert_eq!(polled["status"], "approved");
    assert!(polled["token"].as_str().unwrap().starts_with("wfp_"));
    assert_eq!(
        polled["identity"],
        json!({ "display_name": "kings@example.com", "email": "kings@example.com" })
    );
    let (_, again) = hub.status(code).await;
    assert!(
        again.get("token").is_none(),
        "the token is handed over once"
    );
    assert_eq!(again["identity"]["email"], "kings@example.com");

    // The device acts as Kings.
    let (status, me) = hub
        .call(polled["token"].as_str(), Method::GET, "/api/me", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["id"], kings.id.to_string());
    assert!(hub.device_owner(&other).is_empty());
}

#[tokio::test]
async fn a_code_is_approved_once() {
    let hub = hub().await;
    let (_, first) = hub.person("first@example.com", IdentityRole::Member);
    let (second_person, second) = hub.person("second@example.com", IdentityRole::Member);
    let code = hub.request_code(None).await["code"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(hub.approve_self(&first, &code).await.0, StatusCode::OK);
    let (status, body) = hub.approve_self(&second, &code).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("already been used"));
    assert!(hub.device_owner(&second_person).is_empty());
}

#[tokio::test]
async fn an_expired_code_is_not_found() {
    let hub = hub_with(PairingConfig {
        code_ttl_secs: 0,
        ..Default::default()
    })
    .await;
    let (_, token) = hub.person("kings@example.com", IdentityRole::Member);
    let code = hub.request_code(None).await["code"]
        .as_str()
        .unwrap()
        .to_string();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let (status, body) = hub.approve_self(&token, &code).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("expired"));
    let (status, _) = hub
        .call(
            Some(&token),
            Method::GET,
            &format!("/api/pair/device/pending/{code}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(hub.status(&code).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn approving_needs_a_signed_in_person() {
    let hub = hub().await;
    let code = hub.request_code(None).await["code"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, _) = hub
        .call(
            None,
            Method::POST,
            "/api/pair/device/approve-self",
            Some(json!({ "code": code })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = hub.approve_self(GATEWAY_TOKEN, &code).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the gateway token is no one");
    assert_eq!(hub.status(&code).await.1["status"], "pending");
}

#[tokio::test]
async fn admins_mode_lets_only_admins_connect() {
    let hub = hub_with(PairingConfig {
        who_can_connect: WhoCanConnect::Admins,
        ..Default::default()
    })
    .await;
    let (_, member) = hub.person("member@example.com", IdentityRole::Member);
    let (_, admin) = hub.person("admin@example.com", IdentityRole::SystemAdmin);
    let code = hub.request_code(None).await["code"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = hub.approve_self(&member, &code).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("Only admins"));

    let (status, body) = hub.approve_self(&admin, &code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn only_an_admin_assigns_a_device_to_someone() {
    let hub = hub().await;
    let (_, member) = hub.person("member@example.com", IdentityRole::Member);
    let (_, admin) = hub.person("admin@example.com", IdentityRole::SystemAdmin);
    let (kings, _) = hub.person("kings@example.com", IdentityRole::Member);
    let code = hub.request_code(None).await["code"]
        .as_str()
        .unwrap()
        .to_string();
    let assign = json!({ "code": code, "identity_id": kings.id.to_string() });

    let (status, _) = hub
        .call(
            Some(&member),
            Method::POST,
            "/api/pair/device/approve",
            Some(assign.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(hub.status(&code).await.1["status"], "pending");

    let (status, body) = hub
        .call(
            Some(&admin),
            Method::POST,
            "/api/pair/device/approve",
            Some(assign),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["identity"]["id"], kings.id.to_string());
    assert_eq!(
        hub.status(&code).await.1["identity"]["email"],
        "kings@example.com"
    );

    let (_, devices) = hub
        .call(Some(&admin), Method::GET, "/api/pair/devices", None)
        .await;
    assert_eq!(devices["devices"][0]["owner"]["email"], "kings@example.com");
}

#[tokio::test]
async fn a_person_rejects_a_code_from_the_pair_page() {
    let hub = hub().await;
    let (_, token) = hub.person("kings@example.com", IdentityRole::Member);
    let code = hub.request_code(None).await["code"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, _) = hub
        .call(
            Some(&token),
            Method::POST,
            "/api/pair/device/reject-self",
            Some(json!({ "code": code })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hub.status(&code).await.1["status"], "rejected");
    assert_eq!(
        hub.approve_self(&token, &code).await.0,
        StatusCode::CONFLICT
    );
}

/// The older admin route behaves as it did: it links by the device's hint,
/// auto-provisions an unknown email, and hands the token back.
#[tokio::test]
async fn the_admin_route_still_links_by_hint() {
    let hub = hub().await;
    let hinted = hub.request_code(Some("fresh@example.com")).await;
    let (status, approved) = hub
        .call(
            Some(GATEWAY_TOKEN),
            Method::POST,
            &format!("/api/pair/approve/{}", hinted["code"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(approved["linked_via"], "auto_provisioned");
    assert!(approved["token"].as_str().unwrap().starts_with("wfp_"));
    let created = hub
        .identities
        .get_by_email("fresh@example.com")
        .unwrap()
        .unwrap();
    assert_eq!(approved["linked_identity_id"], created.id.to_string());

    let bare = hub.request_code(None).await;
    let (_, approved) = hub
        .call(
            Some(GATEWAY_TOKEN),
            Method::POST,
            &format!("/api/pair/approve/{}", bare["code"].as_str().unwrap()),
            None,
        )
        .await;
    assert!(approved.get("linked_identity_id").is_none(), "{approved}");
    assert!(approved["token"].as_str().is_some());
}

/// Desk's "Disconnect": a device unpairs itself, and only itself.
#[tokio::test]
async fn a_device_revokes_itself_and_nothing_else() {
    let hub = hub().await;
    let (person, person_token) = hub.person("person@example.com", IdentityRole::Member);
    let mut tokens = Vec::new();
    for _ in 0..2 {
        let code = hub.request_code(None).await["code"]
            .as_str()
            .unwrap()
            .to_string();
        let (status, _) = hub.approve_self(&person_token, &code).await;
        assert_eq!(status, StatusCode::OK);
        let (_, polled) = hub.status(&code).await;
        tokens.push(polled["token"].as_str().unwrap().to_string());
    }

    let (status, revoked) = hub
        .call(
            Some(&tokens[0]),
            Method::POST,
            "/api/pair/device/revoke-self",
            None,
        )
        .await;
    assert_eq!(
        (status, revoked),
        (StatusCode::OK, json!({ "revoked": true }))
    );
    let (gone, _) = hub
        .call(Some(&tokens[0]), Method::GET, "/api/me", None)
        .await;
    assert_eq!(gone, StatusCode::UNAUTHORIZED);
    let (kept, me) = hub
        .call(Some(&tokens[1]), Method::GET, "/api/me", None)
        .await;
    assert_eq!(kept, StatusCode::OK);
    assert_eq!(me["id"], person.id.to_string());
    let (refused, _) = hub
        .call(
            Some(&person_token),
            Method::POST,
            "/api/pair/device/revoke-self",
            None,
        )
        .await;
    assert_eq!(
        refused,
        StatusCode::UNAUTHORIZED,
        "only a device token unpairs a device"
    );
}

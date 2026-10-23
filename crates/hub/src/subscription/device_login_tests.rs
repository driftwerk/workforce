//! Device sign-ins driven against a registry of agents built through
//! composition.

use crate::agent_registry::AgentRegistry;
use crate::hub_core::logins::chatgpt_auth::{Account, AuthJson, TokenSet, CLIENT_ID};
use crate::hub_core::logins::device_login::*;
use crate::serve::Compose as _;
use crate::vault::Vault;
use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use workforce_common::subscription::SubscriptionKind;

use wiremock::matchers::{body_json, body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::subscription::test_support::jwt;

const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
const BOB: &str = "0b7f3d52-8f0a-4c2f-9d44-3c1e6a1f4e22";

fn alice() -> Uuid {
    Uuid::parse_str(ALICE).unwrap()
}

/// Hears every settled flow.
#[derive(Default)]
struct Heard(parking_lot::Mutex<Vec<DeviceFlow>>);

#[async_trait]
impl DeviceLoginListener for Heard {
    async fn settled(&self, flow: DeviceFlow) {
        self.0.lock().push(flow);
    }
}

impl Heard {
    fn states(&self) -> Vec<FlowState> {
        self.0
            .lock()
            .iter()
            .map(|flow| flow.state.clone())
            .collect()
    }
}

/// The tokens a granted sign-in is exchanged for.
fn token_response(refresh_token: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id_token": jwt(serde_json::json!({
            "sub": "user-1",
            "email": "alice@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-1",
                "chatgpt_plan_type": "plus",
            },
        })),
        "access_token": jwt(serde_json::json!({
            "exp": (Utc::now() + TimeDelta::hours(1)).timestamp()
        })),
        "refresh_token": refresh_token,
    }))
}

struct Fixture {
    server: MockServer,
    logins: DeviceLogins,
    vault: Arc<Vault>,
    heard: Arc<Heard>,
    registry: AgentRegistry,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(&dir.path().join("global.enc"), "test-key").unwrap());
        let server = MockServer::start().await;
        let logins = DeviceLogins::new(
            Some(Arc::clone(&vault)),
            DeviceAuthClient::at(reqwest::Client::new(), &server.uri()),
        );
        let heard = Arc::new(Heard::default());
        logins.on_complete(Arc::clone(&heard) as Arc<dyn DeviceLoginListener>);
        Self {
            server,
            logins,
            vault,
            heard,
            registry: AgentRegistry::from_parts(HashMap::new(), String::new()),
            _dir: dir,
        }
    }

    async fn issues(&self, interval: Value) {
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/usercode"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_auth_id": "dev-1",
                "user_code": "ABCD-1234",
                "interval": interval,
            })))
            .mount(&self.server)
            .await;
    }

    async fn polls(&self, status: u16) {
        self.server.reset().await;
        let response = match status {
            200 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "authorization_code": "auth-code-1",
                "code_challenge": "challenge-1",
                "code_verifier": "verifier-1",
            })),
            status => ResponseTemplate::new(status),
        };
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/token"))
            .respond_with(response)
            .mount(&self.server)
            .await;
    }

    async fn exchanges(&self, refresh_token: &str) {
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(token_response(refresh_token))
            .mount(&self.server)
            .await;
    }

    /// The token exchange answers after `delay`, with a signal each time
    /// it is asked.
    async fn exchanges_signalled(
        &self,
        delay: Duration,
    ) -> tokio::sync::mpsc::UnboundedReceiver<()> {
        struct Signalled {
            asked: tokio::sync::mpsc::UnboundedSender<()>,
            response: ResponseTemplate,
        }
        impl wiremock::Respond for Signalled {
            fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
                let _ = self.asked.send(());
                self.response.clone()
            }
        }
        let (asked, signals) = tokio::sync::mpsc::unbounded_channel();
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(Signalled {
                asked,
                response: token_response("rt-late").set_delay(delay),
            })
            .mount(&self.server)
            .await;
        signals
    }

    async fn polls_made(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/api/accounts/deviceauth/token")
            .count()
    }

    fn stored(&self) -> Option<AuthJson> {
        self.vault.sync().unwrap();
        self.vault
            .get(
                &target_slot(
                    &self.registry,
                    &self.vault,
                    &LoginTarget::GlobalUser,
                    alice(),
                )
                .unwrap()
                .key,
            )
            .map(|text| AuthJson::parse(&text).unwrap())
    }

    async fn start(&self) -> DeviceFlow {
        self.logins
            .start_or_join(alice(), LoginTarget::GlobalUser, None)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn device_login_request_shapes_match_codex() {
    let fx = Fixture::new().await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .and(body_json(serde_json::json!({"client_id": CLIENT_ID})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "device_auth_id": "dev-1",
            "usercode": "ABCD-1234",
            "interval": "5",
        })))
        .mount(&fx.server)
        .await;
    let flow = fx.start().await;
    assert_eq!(flow.user_code, "ABCD-1234");
    assert_eq!(
        flow.verification_url,
        format!("{}/codex/device", fx.server.uri())
    );
    assert_eq!(flow.expires_at - flow.started_at, FLOW_LIFETIME);

    fx.server.reset().await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .and(body_json(serde_json::json!({
            "device_auth_id": "dev-1",
            "user_code": "ABCD-1234",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "authorization_code": "auth-code-1",
            "code_challenge": "challenge-1",
            "code_verifier": "verifier-1",
        })))
        .mount(&fx.server)
        .await;
    let redirect = format!("{}/deviceauth/callback", fx.server.uri());
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=auth-code-1"))
        .and(body_string_contains(format!(
            "redirect_uri={}",
            url::form_urlencoded::byte_serialize(redirect.as_bytes()).collect::<String>()
        )))
        .and(body_string_contains(format!("client_id={CLIENT_ID}")))
        .and(body_string_contains("code_verifier=verifier-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id_token": jwt(serde_json::json!({"sub": "user-1", "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1"}})),
            "access_token": jwt(serde_json::json!({"exp": (Utc::now() + TimeDelta::hours(1)).timestamp()})),
            "refresh_token": "rt-signed-in",
        })))
        .mount(&fx.server)
        .await;
    fx.logins.tick(&fx.registry, Utc::now()).await;

    let stored = fx.stored().expect("the sign-in is stored");
    assert_eq!(stored.refresh_token(), Some("rt-signed-in"));
    assert_eq!(stored.account_id().as_deref(), Some("acct-1"));
    assert_eq!(stored.auth_mode.as_deref(), Some("chatgpt"));
    assert!(stored.last_refresh.is_some());
}

#[tokio::test]
async fn device_login_404_is_unsupported_with_paste_pointer() {
    let fx = Fixture::new().await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&fx.server)
        .await;
    let flow = fx.start().await;
    assert_eq!(flow.state, FlowState::Unsupported);
    assert_eq!(
        fx.logins.get(alice(), flow.id).map(|kept| kept.state),
        Some(FlowState::Unsupported),
        "kept, so the card has a flow id to show"
    );
    assert!(unsupported_reason().contains("codex.setup_auth"));
}

#[tokio::test]
async fn zero_interval_is_clamped() {
    let fx = Fixture::new().await;
    fx.issues(serde_json::json!("0")).await;
    assert_eq!(fx.start().await.interval_secs, MIN_INTERVAL_SECS);
    fx.logins
        .cancel(
            alice(),
            fx.logins
                .current(alice(), &LoginTarget::GlobalUser)
                .unwrap()
                .id,
        )
        .await
        .unwrap();
    fx.server.reset().await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "device_auth_id": "dev-2",
            "user_code": "EFGH-5678",
        })))
        .mount(&fx.server)
        .await;
    assert_eq!(
        fx.start().await.interval_secs,
        MIN_INTERVAL_SECS,
        "an absent interval reads as 0 and is clamped too"
    );
}

#[tokio::test]
async fn pending_polls_at_interval_backs_off_and_expires_at_15m() {
    let fx = Fixture::new().await;
    fx.issues(serde_json::json!("5")).await;
    let flow = fx.start().await;
    let at = |secs: i64| flow.started_at + TimeDelta::seconds(secs);
    fx.polls(403).await;

    fx.logins.tick(&fx.registry, at(1)).await;
    assert_eq!(fx.polls_made().await, 1, "polled at once");
    fx.logins.tick(&fx.registry, at(3)).await;
    assert_eq!(fx.polls_made().await, 1, "not before its interval");
    fx.polls(429).await;
    fx.logins.tick(&fx.registry, at(6)).await;
    assert_eq!(
        fx.polls_made().await,
        1,
        "polled at its interval, and throttled"
    );
    fx.logins.tick(&fx.registry, at(12)).await;
    assert_eq!(fx.polls_made().await, 1, "the wait doubled to ten seconds");
    fx.polls(404).await;
    fx.logins.tick(&fx.registry, at(16)).await;
    assert_eq!(fx.polls_made().await, 1, "polled after the doubled wait");
    assert!(fx.heard.states().is_empty(), "still pending");

    fx.logins.tick(&fx.registry, at(15 * 60)).await;
    assert_eq!(fx.heard.states(), [FlowState::Expired]);
    assert_eq!(
        fx.logins.get(alice(), flow.id).map(|kept| kept.state),
        Some(FlowState::Expired)
    );
    fx.logins.tick(&fx.registry, at(15 * 60 + 10)).await;
    assert_eq!(fx.heard.states().len(), 1, "settles once");
    fx.logins.tick(&fx.registry, at(75 * 60 + 1)).await;
    assert!(
        fx.logins.get(alice(), flow.id).is_none(),
        "pruned an hour after"
    );
}

#[tokio::test]
async fn completion_stores_target_slot_fires_listener_once_and_reloads_coding_session() {
    let mut fx = Fixture::new().await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.hub.data_dir = dir.path().display().to_string();
    config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
        .join("memory.db")
        .to_string_lossy()
        .into_owned();
    config.llm.api_key = Some("test-key".into());
    let service = crate::serve::AgentService::from_config(config, None)
        .await
        .unwrap();
    let reloads = Arc::new(AtomicUsize::new(0));
    service.set_coding_session_reload({
        let reloads = Arc::clone(&reloads);
        Arc::new(move || {
            reloads.fetch_add(1, Ordering::SeqCst);
        })
    });
    fx.registry = AgentRegistry::from_parts(
        HashMap::from([("allan".to_string(), service)]),
        "allan".into(),
    );
    fx.issues(serde_json::json!("5")).await;
    let flow = fx.start().await;
    fx.polls(200).await;
    fx.exchanges("rt-signed-in").await;

    fx.logins.tick(&fx.registry, Utc::now()).await;
    fx.logins
        .tick(&fx.registry, Utc::now() + TimeDelta::seconds(30))
        .await;

    assert_eq!(fx.stored().unwrap().refresh_token(), Some("rt-signed-in"));
    assert_eq!(
        fx.heard.states(),
        [FlowState::Completed {
            account: Account {
                email: Some("alice@example.com".into()),
                plan: Some("plus".into()),
            }
        }]
    );
    assert_eq!(fx.heard.0.lock()[0].id, flow.id);
    assert_eq!(reloads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn duplicate_sign_in_fails_flow() {
    let fx = Fixture::new().await;
    fx.issues(serde_json::json!("5")).await;
    let flow = fx.start().await;
    fx.polls(200).await;
    fx.exchanges("rt-elsewhere").await;
    let elsewhere = AuthJson::signed_in(
        TokenSet {
            id_token: jwt(
                serde_json::json!({"sub": "user-1", "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1"}}),
            ),
            access_token: jwt(
                serde_json::json!({"exp": (Utc::now() + TimeDelta::hours(1)).timestamp()}),
            ),
            refresh_token: "rt-elsewhere".into(),
        },
        Utc::now(),
    );
    fx.vault
        .set(SubscriptionKind::ChatGpt.vault_key(), &elsewhere.render())
        .unwrap();

    fx.logins.tick(&fx.registry, Utc::now()).await;

    assert!(fx.stored().is_none());
    let settled = fx.logins.get(alice(), flow.id).unwrap();
    assert!(
        matches!(&settled.state, FlowState::Failed { reason } if reason.contains("already stored")),
        "{:?}",
        settled.state
    );
}

#[tokio::test]
async fn start_is_idempotent_per_target_and_replaces_other_target() {
    let fx = Fixture::new().await;
    fx.issues(serde_json::json!("5")).await;
    let first = fx
        .logins
        .start_or_join(
            alice(),
            LoginTarget::GlobalUser,
            Some(serde_json::json!({"n": 1})),
        )
        .await
        .unwrap();
    let joined = fx
        .logins
        .start_or_join(
            alice(),
            LoginTarget::GlobalUser,
            Some(serde_json::json!({"n": 2})),
        )
        .await
        .unwrap();
    assert_eq!(joined.id, first.id);
    assert_eq!(joined.waiters.len(), 2);
    let rejoined = fx
        .logins
        .start_or_join(
            alice(),
            LoginTarget::GlobalUser,
            Some(serde_json::json!({"n": 2})),
        )
        .await
        .unwrap();
    assert_eq!(rejoined.waiters.len(), 2, "a waiter joins once");
    let issued = |requests: &[wiremock::Request]| {
        requests
            .iter()
            .filter(|r| r.url.path() == "/api/accounts/deviceauth/usercode")
            .count()
    };
    assert_eq!(issued(&fx.server.received_requests().await.unwrap()), 1);

    let other = fx
        .logins
        .start_or_join(
            alice(),
            LoginTarget::AgentUser {
                agent_id: "allan".into(),
            },
            None,
        )
        .await
        .unwrap();
    assert_ne!(other.id, first.id);
    assert_eq!(fx.heard.states(), [FlowState::Replaced]);
    assert_eq!(
        other.waiters,
        [serde_json::json!({"n": 1}), serde_json::json!({"n": 2})],
        "the new flow answers the messages that waited on the one it replaced"
    );
    assert!(fx.logins.get(alice(), first.id).is_none());
    assert_eq!(issued(&fx.server.received_requests().await.unwrap()), 2);
}

#[tokio::test]
async fn cancel_race_discards_grant() {
    let fx = Arc::new(Fixture::new().await);
    fx.issues(serde_json::json!("5")).await;
    let flow = fx.start().await;
    fx.polls(200).await;
    let mut exchanging = fx.exchanges_signalled(Duration::from_millis(300)).await;
    let ticking = tokio::spawn({
        let fx = Arc::clone(&fx);
        async move { fx.logins.tick(&fx.registry, Utc::now()).await }
    });
    exchanging
        .recv()
        .await
        .expect("the grant is being exchanged");
    assert!(fx.logins.cancel(alice(), flow.id).await.unwrap());
    ticking.await.unwrap();

    assert!(fx.stored().is_none(), "the late grant is dropped unstored");
    assert_eq!(fx.heard.states(), [FlowState::Cancelled]);
}

#[tokio::test]
async fn cancel_is_not_held_up_by_a_sign_in_waiting_on_its_slot() {
    let fx = Arc::new(Fixture::new().await);
    fx.issues(serde_json::json!("5")).await;
    let flow = fx.start().await;
    fx.polls(200).await;
    let mut exchanging = fx.exchanges_signalled(Duration::ZERO).await;
    let slot = target_slot(&fx.registry, &fx.vault, &LoginTarget::GlobalUser, alice()).unwrap();
    let held = slot.lock().await.unwrap();
    let ticking = tokio::spawn({
        let fx = Arc::clone(&fx);
        async move { fx.logins.tick(&fx.registry, Utc::now()).await }
    });
    exchanging
        .recv()
        .await
        .expect("the grant is being exchanged");

    let cancelled =
        tokio::time::timeout(Duration::from_secs(10), fx.logins.cancel(alice(), flow.id))
            .await
            .expect("a cancel never waits on a slot's lock");
    assert!(cancelled.unwrap());
    drop(held);
    ticking.await.unwrap();

    assert!(
        fx.stored().is_none(),
        "a sign-in for a cancelled flow is not kept"
    );
    assert_eq!(fx.heard.states(), [FlowState::Cancelled]);
}

#[tokio::test]
async fn transient_exchange_failure_retries_and_refusal_fails() {
    let fx = Fixture::new().await;
    fx.issues(serde_json::json!("5")).await;
    let flow = fx.start().await;
    fx.polls(200).await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&fx.server)
        .await;
    fx.exchanges("rt-retried").await;

    fx.logins.tick(&fx.registry, flow.started_at).await;
    assert!(fx.heard.states().is_empty(), "still pending after a 503");
    assert_eq!(
        fx.logins.get(alice(), flow.id).map(|kept| kept.state),
        Some(FlowState::Pending)
    );
    fx.logins
        .tick(&fx.registry, flow.started_at + TimeDelta::seconds(5))
        .await;
    assert!(fx.heard.states().is_empty(), "the retry backs off");
    fx.logins
        .tick(&fx.registry, flow.started_at + TimeDelta::seconds(10))
        .await;
    assert_eq!(fx.stored().unwrap().refresh_token(), Some("rt-retried"));

    let fx = Fixture::new().await;
    fx.issues(serde_json::json!("5")).await;
    let flow = fx.start().await;
    fx.polls(200).await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&fx.server)
        .await;
    fx.logins.tick(&fx.registry, flow.started_at).await;
    assert!(
        matches!(&fx.heard.states()[..], [FlowState::Failed { reason }] if reason.contains("400")),
        "{:?}",
        fx.heard.states()
    );
}

#[tokio::test]
async fn polling_forgets_flows_settled_elsewhere() {
    let fx = Fixture::new().await;
    fx.issues(serde_json::json!("5")).await;
    let flow = fx.start().await;
    fx.polls(403).await;
    fx.logins.tick(&fx.registry, flow.started_at).await;
    assert!(fx.logins.is_due(flow.id));

    let other_replica = DeviceLogins::new(
        Some(Arc::clone(&fx.vault)),
        DeviceAuthClient::at(reqwest::Client::new(), &fx.server.uri()),
    );
    assert!(other_replica.cancel(alice(), flow.id).await.unwrap());
    fx.logins
        .tick(&fx.registry, flow.started_at + TimeDelta::seconds(1))
        .await;

    assert!(fx.logins.none_due());
}

#[tokio::test]
async fn flow_poll_404s_for_other_identity() {
    let fx = Fixture::new().await;
    fx.issues(serde_json::json!("5")).await;
    let flow = fx.start().await;
    let bob = Uuid::parse_str(BOB).unwrap();
    assert!(fx.logins.get(bob, flow.id).is_none());
    assert!(fx.logins.current(bob, &LoginTarget::GlobalUser).is_none());
    assert!(!fx.logins.cancel(bob, flow.id).await.unwrap());
    assert!(fx.logins.get(alice(), flow.id).is_some());
}

#[test]
fn agent_cannot_read_device_login_record() {
    let record = format!("{RECORD_PREFIX}{ALICE}");
    for uid in [Some(ALICE), Some("system"), None] {
        assert_eq!(
            crate::vault_policy::agent_can_read(&record, uid),
            Err(crate::vault_policy::AgentReadDenial::DeviceLogin),
            "{uid:?}"
        );
    }
    assert!(
        crate::vault_policy::is_org_wide(&record),
        "agents can't write it"
    );
}

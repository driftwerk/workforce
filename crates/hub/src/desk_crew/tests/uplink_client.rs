//! The engine's uplink (remote bosses, RB5b): devbox holds Allan's link
//! for Acme, proves it to the boss hub it dials, and serves Allan's calls
//! under Acme's grant, each in order, on time and within its limits.

use super::*;
use crate::boss_links::engine_key::{verify_challenge, Challenge};
use crate::desk_uplink::{self, call_refused, Ended, HeldLink, Uplink};
use futures::StreamExt;

/// The hub's own token, as Desk's native side holds it.
struct HubToken;

#[async_trait::async_trait]
impl Authorize for HubToken {
    async fn authorize(
        &self,
        _: &HeaderMap,
        _: Option<&str>,
    ) -> Result<Option<String>, StatusCode> {
        Ok(None)
    }
    fn is_hub_token(&self, _: &HeaderMap) -> bool {
        true
    }
}

const BOSS_HUB: &str = "https://boss.example.com";

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    bearer: &str,
    body: Value,
) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {bearer}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A gateway state with a vault of its own, for the engine's links.
fn with_vault(dir: &Path) -> Arc<GatewayState> {
    let fixture = crate::remote_mcp::testing::fixture(false);
    let vault = crate::vault::Vault::open(
        &dir.join("vault.json"),
        &crate::vault::Vault::generate_master_key(),
    )
    .unwrap();
    Arc::new(GatewayState {
        vault: Some(Arc::new(vault)),
        ..Arc::try_unwrap(fixture.state)
            .ok()
            .expect("the fixture's only handle")
    })
}

#[test]
fn a_call_is_taken_only_in_order_and_on_time() {
    let now = chrono::Utc::now();
    assert_eq!(call_refused(1, 0, Some(now), now), None);
    assert!(call_refused(1, 1, Some(now), now).is_some());
    assert!(call_refused(2, 1, Some(now - chrono::Duration::seconds(121)), now).is_some());
    assert!(call_refused(2, 1, Some(now + chrono::Duration::seconds(121)), now).is_some());
    assert!(call_refused(2, 1, None, now).is_some());
    for tries in 0..10 {
        let b = desk_uplink::backoff(tries);
        assert!(b >= std::time::Duration::from_secs(1) && b <= std::time::Duration::from_secs(75));
    }
    assert!(desk_uplink::backoff(0) < std::time::Duration::from_secs(2));
    assert!(desk_uplink::backoff(20) >= std::time::Duration::from_secs(60));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_proves_its_link_and_serves_its_bosss_calls_in_order() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_any, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = false\n",
            &[],
        )
        .await;
        let mine = new_session(&d, dir.path(), "acme-api").await;
        let item = workforce_desk::session_item(mine.id);
        d.change_layout(|l| l.create("acme", "Acme", std::slice::from_ref(&item), None))
            .await
            .unwrap();
        let desk_app = workforce_desk::sessions_router(d.clone(), Arc::new(HubToken));
        let (code, out) = send(
            &desk_app,
            "PUT",
            "/api/desk/grants/acme",
            "hub",
            json!({"grantVersion": 1, "revision": 1, "name": "Acme",
                   "boss": "remote:driftwerk/allan", "level": "act", "sees": "replies",
                   "members": [mine.id]}),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{out}");

        // Desk hands devbox the link: kept, never answered back.
        let state = with_vault(dir.path());
        let app = desk_uplink::routes(Arc::clone(&state));
        let token = state.auth_token.clone();
        let link_id = uuid::Uuid::new_v4();
        let token_id = uuid::Uuid::new_v4();
        let secret = format!("wfb_{}", "abcdefghij".repeat(4));
        let link = |url: &str| {
            json!({"hub": "driftwerk", "hubUrl": url, "canonicalHub": url, "linkId": link_id,
                   "tokenId": token_id, "token": secret, "engineId": "devbox", "agentId": "allan",
                   "expiresAt": chrono::Utc::now() + chrono::Duration::days(1)})
        };
        let (code, _) = send(
            &app,
            "PUT",
            "/api/desk/boss-links/acme",
            "not-it",
            link(BOSS_HUB),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let (code, _) = send(
            &app,
            "PUT",
            "/api/desk/boss-links/acme?token=x",
            &token,
            link(BOSS_HUB),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let (code, _) = send(
            &app,
            "PUT",
            "/api/desk/boss-links/acme",
            &token,
            link("http://boss.example.com"),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "https alone");
        for private in ["https://127.0.0.1", "https://10.0.0.5:8443", "https://[::1]", "https://169.254.1.1", "https://localhost"] {
            let (code, _) = send(&app, "PUT", "/api/desk/boss-links/acme", &token, link(private)).await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "{private}");
        }
        let (code, _) = send(
            &app,
            "PUT",
            "/api/desk/boss-links/acme",
            &token,
            link(BOSS_HUB),
        )
        .await;
        assert_eq!(code, StatusCode::NO_CONTENT);
        // Another hub can't take the name this one has here.
        let mut other = link("https://other.example.com");
        other["linkId"] = json!(uuid::Uuid::new_v4());
        let (code, _) = send(&app, "PUT", "/api/desk/boss-links/acme", &token, other).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "one name, one hub");
        let (_, listed) = send(&app, "GET", "/api/desk/boss-links", &token, json!({})).await;
        assert_eq!(listed["links"][0]["linkId"], json!(link_id));
        assert!(!listed.to_string().contains(&secret), "never a token");
        let (_, key) = send(&app, "GET", "/api/desk/engine-key", &token, json!({})).await;
        let public = crate::boss_links::engine_key::EnginePublicKey::parse(
            key["publicKey"].as_str().unwrap(),
        )
        .unwrap();
        let (_, again) = send(&app, "GET", "/api/desk/engine-key", &token, json!({})).await;
        assert_eq!(again, key, "made once, kept");

        // The boss hub: it says another hub's name in its challenge, which
        // devbox doesn't sign; it checks the proof for its own.
        let (said_tx, mut said) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let calls = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
        let boss_hub = {
            let calls = Arc::clone(&calls);
            axum::Router::new().route(
                "/ws/desk-boss",
                axum::routing::get(move |ws: axum::extract::WebSocketUpgrade, h: HeaderMap| {
                    let (said_tx, calls) = (said_tx.clone(), Arc::clone(&calls));
                    async move {
                        assert_eq!(h["desk-boss-protocol"], "1");
                        assert!(h["authorization"]
                            .to_str()
                            .unwrap()
                            .starts_with("Bearer wfb_"));
                        ws.on_upgrade(move |mut s| async move {
                            use axum::extract::ws::Message as M;
                            let nonce = [7u8; 32];
                            let b64 = |b: &[u8]| {
                                base64::Engine::encode(
                                    &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                                    b,
                                )
                            };
                            let _ = s
                                .send(M::Text(
                                    json!({"type": "challenge", "nonce": b64(&nonce),
                                "hub": "https://elsewhere.example.com"})
                                    .to_string()
                                    .into(),
                                ))
                                .await;
                            let Some(Ok(M::Text(t))) = s.next().await else {
                                return;
                            };
                            let hello: Value = serde_json::from_str(&t).unwrap();
                            let sig = base64::Engine::decode(
                                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                                hello["links"][0]["signature"].as_str().unwrap(),
                            )
                            .unwrap();
                            let proved = verify_challenge(
                                &public,
                                Challenge {
                                    nonce: &nonce,
                                    token_id,
                                    hub: BOSS_HUB,
                                    engine_id: "devbox",
                                },
                                &sig,
                            );
                            let _ = said_tx.send(json!({"proved": proved, "hello": hello}));
                            let _ = s
                                .send(M::Text(
                                    json!({"type": "ready",
                                "links": [{"linkId": link_id}], "refused": []})
                                    .to_string()
                                    .into(),
                                ))
                                .await;
                            for c in calls.lock().await.iter() {
                                let _ = s.send(M::Text(c.to_string().into())).await;
                            }
                            while let Some(Ok(m)) = s.next().await {
                                if let M::Text(t) = m {
                                    let v: Value = serde_json::from_str(&t).unwrap();
                                    let _ = said_tx.send(v);
                                }
                            }
                        })
                    }
                }),
            )
        };
        let now = chrono::Utc::now();
        let call = |id: &str, seq: u64, at: chrono::DateTime<chrono::Utc>, link: uuid::Uuid| {
            json!({"type": "call", "callId": id, "seq": seq, "linkId": link,
                   "tool": "crew.overview", "input": {}, "sentAt": at})
        };
        *calls.lock().await = vec![
            call("first", 1, now, link_id),
            call("again", 1, now, link_id),
            call("stale", 2, now - chrono::Duration::minutes(10), link_id),
            call("not-ours", 3, now, uuid::Uuid::new_v4()),
            call("second", 4, now, link_id),
            call("over", 5, now, link_id),
            call("last", 6, now, link_id),
        ];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, boss_hub).await.unwrap() });

        let held: Vec<HeldLink> = desk_uplink::held_links(state.vault.as_ref().unwrap());
        let uplink = Uplink {
            state: Arc::clone(&state),
            crew: Arc::clone(&crew),
            limits: workforce_common::settings::DeskBossesConfig {
                calls_per_minute: 2,
                ..Default::default()
            },
            dial: Some(format!("ws://{addr}")),
            rates: Default::default(),
        };
        let reconnecting = uplink.clone();
        let held_again = held.clone();
        let running = tokio::spawn(async move {
            uplink
                .connect_once(
                    BOSS_HUB,
                    &held,
                    Some(chrono::Utc::now() - chrono::Duration::hours(1)),
                )
                .await
        });

        let first = said.recv().await.unwrap();
        assert_eq!(
            first["proved"],
            json!(true),
            "signed for the hub it dialed: {first}"
        );
        assert_eq!(first["hello"]["engine"]["id"], "devbox");
        assert!(first["hello"]["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("crew.overview")));
        let mut results = std::collections::HashMap::new();
        while results.len() < 9 {
            let v = tokio::time::timeout(std::time::Duration::from_secs(10), said.recv())
                .await
                .expect("the engine's answers")
                .unwrap();
            let key = v["callId"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v["type"].as_str().unwrap().to_string());
            results.insert(key, v);
        }
        assert_eq!(results["asks_open"]["asks"], json!([]));
        assert_eq!(results["catch_up"]["links"][0]["linkId"], json!(link_id));
        assert_eq!(results["first"]["status"], "ok", "{results:?}");
        assert!(results["first"]["body"]["sessions"]
            .as_str()
            .unwrap()
            .contains(&mine.id.to_string()));
        assert!(results["again"]["body"]["error"]
            .as_str()
            .unwrap()
            .contains("out of order"));
        assert!(results["stale"]["body"]["error"]
            .as_str()
            .unwrap()
            .contains("clock"));
        assert_eq!(results["not-ours"]["status"], "refused");
        assert_eq!(results["second"]["status"], "ok");
        assert!(
            results["over"]["body"]["error"]
                .as_str()
                .unwrap()
                .contains("a minute"),
            "{results:?}"
        );
        assert_eq!(results["last"]["status"], "refused");
        running.abort();

        // Connected again, the link's limits are where they were.
        *calls.lock().await = vec![call("after", 1, chrono::Utc::now(), link_id)];
        let running = tokio::spawn(async move {
            reconnecting.connect_once(BOSS_HUB, &held_again, None).await
        });
        let mut after = None;
        while after.is_none() {
            let v = tokio::time::timeout(std::time::Duration::from_secs(10), said.recv())
                .await
                .expect("the engine's answer")
                .unwrap();
            if v["callId"] == "after" {
                after = Some(v);
            }
        }
        let after = after.unwrap();
        assert!(after["body"]["error"].as_str().unwrap().contains("a minute"), "{after}");
        running.abort();

        // Starts are counted from what this engine recorded: a fresh socket,
        // even a restarted engine, makes no room.
        let row = workforce_desk::grant_store::CallRow {
            caller: "boss:driftwerk/allan".into(),
            boss: None,
            group: Some("acme".into()),
            tool: "crew.start_session",
            input: "{}".into(),
            input_hash: String::new(),
            outcome: "pending",
            said: None,
            result: json!({}),
        };
        let store = d.store();
        if let workforce_desk::grant_store::Begun::New(id) = store
            .boss_call_begin(Some(&link_id.to_string()), Some("earlier-start"), &row)
            .await
            .unwrap()
        {
            store.boss_call_end(id, "ok", None, &json!({})).await.unwrap();
        }
        *calls.lock().await = vec![json!({"type": "call", "callId": "start", "seq": 1,
            "linkId": link_id, "tool": "crew.start_session",
            "input": {"folder": "/tmp", "goal": "g", "reason": "r"}, "sentAt": chrono::Utc::now()})];
        let fresh = Uplink {
            state: Arc::clone(&state),
            crew: Arc::clone(&crew),
            limits: workforce_common::settings::DeskBossesConfig {
                starts_per_hour: 1,
                ..Default::default()
            },
            dial: Some(format!("ws://{addr}")),
            rates: Default::default(),
        };
        let held_once_more = desk_uplink::held_links(state.vault.as_ref().unwrap());
        let running =
            tokio::spawn(async move { fresh.connect_once(BOSS_HUB, &held_once_more, None).await });
        let mut start = None;
        while start.is_none() {
            let v = tokio::time::timeout(std::time::Duration::from_secs(10), said.recv())
                .await
                .expect("the engine's answer")
                .unwrap();
            if v["callId"] == "start" {
                start = Some(v);
            }
        }
        let start = start.unwrap();
        assert!(start["body"]["error"].as_str().unwrap().contains("an hour"), "{start}");
        running.abort();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hub_that_refuses_the_link_marks_it_revoked() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let state = with_vault(dir.path());
        let d = desk(pool, dir.path()).await;
        let crew = CrewHub::start(Arc::clone(&state), d, FAST, CancellationToken::new());
        let refusing = axum::Router::new().route(
            "/ws/desk-boss",
            axum::routing::get(|| async { StatusCode::UNAUTHORIZED }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, refusing).await.unwrap() });
        let held: HeldLink = serde_json::from_value(json!({
            "group": "acme", "hub": "driftwerk", "hubUrl": BOSS_HUB,
            "linkId": uuid::Uuid::new_v4(), "tokenId": uuid::Uuid::new_v4(),
            "token": format!("wfb_{}", "abcdefghij".repeat(4)),
            "expiresAt": chrono::Utc::now() + chrono::Duration::days(1),
            "engineId": "devbox", "agentId": "allan"}))
        .unwrap();
        let uplink = Uplink {
            state: Arc::clone(&state),
            crew,
            limits: Default::default(),
            dial: Some(format!("ws://{addr}")),
            rates: Default::default(),
        };
        assert_eq!(
            uplink.connect_once(BOSS_HUB, &[held], None).await,
            Ended::Revoked
        );
    })
    .await;
}

/// The uplink's TLS is the hub's rustls, with the system's roots: dialing a
/// `wss` address that isn't there is an error, never a panic.
#[tokio::test]
async fn a_wss_dial_uses_the_hubs_tls() {
    workforce_common::tls::install_default_crypto_provider();
    let dialed = tokio_tungstenite::connect_async("wss://127.0.0.1:9/ws/desk-boss").await;
    assert!(dialed.is_err());
    assert!(desk_uplink::public_address("https://boss.example.com"));
    assert!(!desk_uplink::public_address("https://192.168.1.4"));
    assert!(!desk_uplink::public_address("https://100.64.0.1"));
    assert!(!desk_uplink::public_address("https://[fd00::1]"));
}

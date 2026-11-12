//! The hub's end of Desk's extension, against a stand-in extension that
//! speaks the protocol (`protocol-v1.json`, the same file Desk's extension
//! tests read) with a real P-256 key.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use futures::{SinkExt, StreamExt};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use super::*;

const FIXTURE: &str = include_str!("protocol-v1.json");

/// `f` in a turn of the hub's holder.
async fn held<T>(f: impl std::future::Future<Output = T>) -> T {
    crate::desk_computer::sarah_holds();
    workforce_agent::runner::CURRENT_USER_ID
        .scope("sarah".into(), f)
        .await
}

/// A stand-in extension: a key, and a socket to the hub's end.
struct Ext {
    key: EcdsaKeyPair,
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl Ext {
    fn public(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.key.public_key().as_ref())
    }

    async fn say(&mut self, m: Value) {
        self.ws.send(Message::text(m.to_string())).await.unwrap();
    }

    async fn hear(&mut self) -> Value {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("the hub answers")
                .expect("open")
                .unwrap();
            if let Message::Text(t) = m {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] != "status" {
                    return v;
                }
            }
        }
    }

    /// Says hello and signs the challenge; what the hub said next.
    async fn handshake(&mut self, sign_right: bool) -> (Value, Value) {
        let key = self.public();
        self.say(json!({ "type": "hello", "protocol": 1, "version": "0.1.0", "key": key, "label": "ignored" }))
            .await;
        let challenge = self.hear().await;
        let nonce = challenge["nonce"].as_str().unwrap().to_string();
        let message = if sign_right {
            format!("{PROOF_PREFIX}{nonce}")
        } else {
            format!("other:{nonce}")
        };
        let sig = self
            .key
            .sign(&SystemRandom::new(), message.as_bytes())
            .unwrap();
        self.say(json!({ "type": "proof", "signature": base64::engine::general_purpose::STANDARD.encode(sig.as_ref()) }))
            .await;
        (challenge, self.hear().await)
    }
}

fn new_key() -> EcdsaKeyPair {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap()
}

/// The hub's end on a local port, serving every connection (the auth in
/// front of it is the gateway's, tested there).
async fn hub(chrome: Arc<Chrome>) -> u16 {
    let app = axum::Router::new().route(
        "/ws/desk/chrome",
        axum::routing::get(move |ws: axum::extract::ws::WebSocketUpgrade| {
            let chrome = Arc::clone(&chrome);
            async move { ws.on_upgrade(move |s| chrome.serve(s)) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

async fn ext(port: u16, key: EcdsaKeyPair) -> Ext {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/desk/chrome"))
        .await
        .unwrap();
    Ext { key, ws }
}

#[test]
fn the_protocol_is_the_extensions() {
    let f: Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(f["protocol"].as_u64(), Some(PROTOCOL));
    assert_eq!(f["proof_prefix"], PROOF_PREFIX);
    assert_eq!(f["hub_path"], "/ws/desk/chrome");
    let cmds: Vec<&str> = f["from_hub"]["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["cmd"].as_str().unwrap())
        .collect();
    for used in [
        "open",
        "navigate",
        "where",
        "read",
        "shown",
        "click",
        "fill",
        "type",
        "press",
        "select",
        "wait",
        "upload",
        "capture",
        "capture_key",
        "front",
        "close",
        "stop",
    ] {
        assert!(cmds.contains(&used), "{used} is in the protocol");
    }
}

#[test]
fn a_keys_code_is_the_one_the_extension_shows() {
    // The extension's labelOf: SHA-256 of the raw key, upper-case hex, four
    // groups of four.
    let key = [4u8; 65];
    let label = label_of(&key);
    assert!(
        regex::Regex::new(r"^[0-9A-F]{4}(-[0-9A-F]{4}){3}$")
            .unwrap()
            .is_match(&label),
        "{label}"
    );
    assert_ne!(label, label_of(&[5u8; 65]));
}

#[tokio::test]
async fn a_chrome_reaches_nothing_until_the_person_confirms_it_and_then_takes_calls() {
    let dir = tempfile::tempdir().unwrap();
    let chrome = Chrome::new(dir.path().join("chrome-profiles.json"));
    let port = hub(Arc::clone(&chrome)).await;
    let mut e = ext(port, new_key()).await;
    let (challenge, next) = e.handshake(true).await;
    assert_eq!(challenge["state"], "pending");
    assert_eq!(next["type"], "waiting");
    // The person sees it asking, with its code.
    let view = chrome.view();
    assert_eq!(view.len(), 1);
    assert_eq!(view[0].state, "pending");
    assert_eq!(
        view[0].label,
        label_of(
            &base64::engine::general_purpose::STANDARD
                .decode(e.public())
                .unwrap()
        )
    );
    assert!(!chrome.connected(), "no calls before it is confirmed");
    assert!(held(chrome.call("where", json!({ "session": "s" })))
        .await
        .is_err());
    // Confirmed: ready, and calls go to it.
    assert!(chrome.confirm(&view[0].id));
    assert_eq!(e.hear().await["type"], "ready");
    let asked = tokio::spawn({
        let chrome = Arc::clone(&chrome);
        async move { held(chrome.call("where", json!({ "session": "s" }))).await }
    });
    let call = e.hear().await;
    assert_eq!(call["type"], "call");
    assert_eq!(call["cmd"], "where");
    e.say(json!({ "type": "result", "id": call["id"], "ok": true, "value": { "url": "https://shop.example/", "title": "Shop" } }))
        .await;
    assert_eq!(asked.await.unwrap().unwrap()["title"], "Shop");
    // A failure comes back as the extension said it.
    let asked = tokio::spawn({
        let chrome = Arc::clone(&chrome);
        async move { held(chrome.call("click", json!({ "session": "s", "selector": "#x" }))).await }
    });
    let call = e.hear().await;
    e.say(json!({ "type": "result", "id": call["id"], "ok": false, "error": "Nothing on the page matches `#x`." }))
        .await;
    assert!(asked
        .await
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("Nothing on the page matches"));
    // Kept: a new hub reads it confirmed.
    let again = Chrome::new(dir.path().join("chrome-profiles.json"));
    assert_eq!(again.view().len(), 1);
    assert_eq!(again.view()[0].state, "confirmed");
}

#[tokio::test]
async fn a_confirmed_chrome_is_ready_at_once_and_a_second_connection_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let chrome = Chrome::new(dir.path().join("p.json"));
    let port = hub(Arc::clone(&chrome)).await;
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let key = || {
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap()
    };
    let mut first = ext(port, key()).await;
    first.handshake(true).await;
    let id = chrome.view()[0].id.clone();
    chrome.confirm(&id);
    assert_eq!(first.hear().await["type"], "ready");
    // The same profile again, while the first is live: refused.
    let mut second = ext(port, key()).await;
    let (challenge, next) = second.handshake(true).await;
    assert_eq!(challenge["state"], "confirmed");
    assert_eq!(next["type"], "refused");
    assert!(chrome.connected(), "the first stays");
    // After the first goes, the profile is ready again at once.
    drop(first);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut third = ext(port, key()).await;
    let (_, next) = third.handshake(true).await;
    assert_eq!(next["type"], "ready");
}

#[tokio::test]
async fn a_connection_that_cant_prove_its_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let chrome = Chrome::new(dir.path().join("p.json"));
    let port = hub(Arc::clone(&chrome)).await;
    let mut e = ext(port, new_key()).await;
    let (_, next) = e.handshake(false).await;
    assert_eq!(next["type"], "refused");
    assert!(chrome.view().is_empty(), "nothing is asked of the person");
}

#[tokio::test]
async fn refusing_or_forgetting_a_chrome_ends_it() {
    let dir = tempfile::tempdir().unwrap();
    let chrome = Chrome::new(dir.path().join("p.json"));
    let port = hub(Arc::clone(&chrome)).await;
    let mut e = ext(port, new_key()).await;
    e.handshake(true).await;
    let id = chrome.view()[0].id.clone();
    assert!(chrome.forget(&id));
    assert_eq!(e.hear().await["type"], "refused");
    assert!(chrome.view().is_empty());
}

#[test]
fn only_the_keys_own_signature_verifies() {
    let k = new_key();
    let public = k.public_key().as_ref().to_vec();
    let sig = k
        .sign(&SystemRandom::new(), format!("{PROOF_PREFIX}n1").as_bytes())
        .unwrap();
    let b64 = base64::engine::general_purpose::STANDARD.encode(sig.as_ref());
    assert!(verify(&public, "n1", &b64));
    assert!(!verify(&public, "n2", &b64), "another challenge");
    assert!(
        !verify(new_key().public_key().as_ref(), "n1", &b64),
        "another key"
    );
    assert!(!verify(&public, "n1", "not base64!"));
}

#[tokio::test]
async fn a_call_in_someone_elses_turn_never_reaches_the_extension() {
    let dir = tempfile::tempdir().unwrap();
    let chrome = Chrome::new(dir.path().join("p.json"));
    crate::desk_computer::sarah_holds();
    let theirs = workforce_agent::runner::CURRENT_USER_ID
        .scope(
            "tom".into(),
            chrome.call("where", json!({ "session": "s" })),
        )
        .await
        .unwrap_err();
    assert_eq!(theirs.to_string(), OTHERS_TURN);
    let nobody = chrome
        .call("click", json!({ "session": "s" }))
        .await
        .unwrap_err();
    assert_eq!(nobody.to_string(), OTHERS_TURN);
    // Letting tabs go isn't someone's use of the browser.
    let close = chrome
        .call("close", json!({ "session": "s" }))
        .await
        .unwrap_err();
    assert_ne!(close.to_string(), OTHERS_TURN);
}

/// A second Chrome the person confirms doesn't take over from the one they
/// use: Settings shows both, and the first stays chosen until they choose.
#[tokio::test]
async fn confirming_another_chrome_keeps_the_one_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let chrome = Chrome::new(dir.path().join("p.json"));
    let port = hub(Arc::clone(&chrome)).await;
    let mut first = ext(port, new_key()).await;
    first.handshake(true).await;
    let one = chrome.view()[0].id.clone();
    assert!(chrome.confirm(&one));
    assert_eq!(first.hear().await["type"], "ready");
    let mut second = ext(port, new_key()).await;
    second.handshake(true).await;
    let two = chrome
        .view()
        .into_iter()
        .find(|p| p.state == "pending")
        .unwrap()
        .id;
    assert!(chrome.confirm(&two));
    let chosen: Vec<String> = chrome
        .view()
        .into_iter()
        .filter(|p| p.chosen)
        .map(|p| p.id)
        .collect();
    assert_eq!(chosen, std::slice::from_ref(&one));
    assert!(chrome.choose(&two));
    assert!(chrome.view().iter().any(|p| p.id == two && p.chosen));
}

/// No more than a few Chromes ask the person at once.
#[tokio::test]
async fn only_a_few_chromes_ask_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let chrome = Chrome::new(dir.path().join("p.json"));
    let port = hub(Arc::clone(&chrome)).await;
    let mut asking = Vec::new();
    for _ in 0..MAX_ASKING {
        let mut e = ext(port, new_key()).await;
        let (_, next) = e.handshake(true).await;
        assert_eq!(next["type"], "waiting");
        asking.push(e);
    }
    let mut more = ext(port, new_key()).await;
    let (_, next) = more.handshake(true).await;
    assert_eq!(next["type"], "refused");
    assert_eq!(chrome.view().len(), MAX_ASKING);
}

#[test]
fn a_version_is_shown_only_as_digits_and_dots() {
    assert_eq!(version_of(Some("1.2.3")), "1.2.3");
    for said in [
        None,
        Some(""),
        Some("1.0 — Official Desk, safe to confirm"),
        Some("12345678901234567"),
        Some("1.0\u{202e}"),
    ] {
        assert_eq!(version_of(said), "?", "{said:?}");
    }
}

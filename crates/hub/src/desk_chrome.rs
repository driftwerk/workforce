//! Desk's Chrome extension, as the hub reaches it: `GET /ws/desk/chrome`,
//! which Desk Helper opens for the extension as its native messaging host,
//! with Desk's device token in the header.
//!
//! The token says the connection comes from this computer's Desk, but
//! anything that can start Desk Helper can open one, and an extension's id
//! is public. So each Chrome profile is confirmed by the person once, in
//! Settings → Browser (the code it shows there matches the one the
//! extension's popup shows), and remembered by its public key; every
//! connection signs a fresh challenge with that profile's key, and gets no
//! call until the profile is confirmed and the signature verifies. A second
//! live connection for a profile is refused, not swapped in.
//!
//! The protocol is the extension's (`extension/protocol/v1.json` in Desk's
//! repo): hello, challenge, proof, then ready or waiting; calls with ids,
//! their results; the extension's events (a tab of Desk's closed, its own
//! Stop); and what Desk is doing, for its popup.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use eyre::{eyre, Result};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{info, warn};
use workforce_skills::adapters::browser::{self, ExtensionLink};

use crate::gateway::{authenticate_ws, ws_token, GatewayState};

/// The protocol's version.
pub const PROTOCOL: u64 = 1;
/// What the signature over a challenge covers: this, then the nonce.
pub const PROOF_PREFIX: &str = "desk-chrome-v1:";
/// Desk's extension in the Chrome Web Store, once it is there; until then
/// "Add Desk to Chrome" says it isn't yet.
pub const STORE_ID: &str = "";

/// How long the extension has to say hello and prove itself.
const HANDSHAKE: Duration = Duration::from_secs(15);

/// One Chrome profile the extension connected from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    /// From its key: the first 16 hex digits of the key's SHA-256.
    pub id: String,
    /// Its public key, raw P-256, base64.
    pub key: String,
    /// The code the person compares: four groups of four.
    pub label: String,
    pub version: String,
    pub confirmed: bool,
    /// The one Desk uses.
    #[serde(default)]
    pub chosen: bool,
}

/// What Settings → Browser shows of a profile.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProfileView {
    pub id: String,
    pub label: String,
    pub version: String,
    pub state: &'static str,
    pub connected: bool,
    pub chosen: bool,
}

/// Where a call's answer goes: its value, or what the extension said.
type Answer = oneshot::Sender<std::result::Result<Value, String>>;

/// A live connection: where its calls go, and the calls waiting on it.
struct Conn {
    out: mpsc::UnboundedSender<String>,
    waiting: Arc<parking_lot::Mutex<HashMap<u64, Answer>>>,
}

/// The hub's end of Desk's extension.
pub struct Chrome {
    file: PathBuf,
    profiles: parking_lot::Mutex<Vec<Profile>>,
    /// The ready connection of each confirmed profile.
    live: parking_lot::Mutex<HashMap<String, Arc<Conn>>>,
    /// Profiles asking now, waiting for the person.
    asking: parking_lot::Mutex<std::collections::HashSet<String>>,
    /// Changes when a profile is confirmed or forgotten.
    changed: watch::Sender<u64>,
    next: AtomicU64,
}

static CHROME: OnceLock<Arc<Chrome>> = OnceLock::new();

/// The hub's end of the extension, once Desk's browser is installed.
pub fn installed() -> Option<&'static Arc<Chrome>> {
    CHROME.get()
}

/// The 16-hex id of a public key.
fn id_of(key: &[u8]) -> String {
    hex::encode(&Sha256::digest(key)[..8])
}

/// The code the person compares, from a public key: as the extension
/// computes it (`identity.ts`'s `labelOf`).
pub fn label_of(key: &[u8]) -> String {
    let hex = hex::encode_upper(Sha256::digest(key));
    [0, 4, 8, 12].map(|i| &hex[i..i + 4]).join("-")
}

/// Whether `signature` (P1363, base64) is `key`'s over the challenge
/// `nonce`.
pub fn verify(key: &[u8], nonce: &str, signature: &str) -> bool {
    let Ok(sig) = base64::engine::general_purpose::STANDARD.decode(signature) else {
        return false;
    };
    let message = format!("{PROOF_PREFIX}{nonce}");
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, key)
        .verify(message.as_bytes(), &sig)
        .is_ok()
}

impl Chrome {
    /// The hub's end, its confirmed profiles kept in `file` (in Desk's own
    /// folder, which no agent's tools reach).
    pub fn new(file: PathBuf) -> Arc<Self> {
        let profiles = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Arc::new(Self {
            file,
            profiles: parking_lot::Mutex::new(profiles),
            live: Default::default(),
            asking: Default::default(),
            changed: watch::channel(0).0,
            next: AtomicU64::new(1),
        })
    }

    fn save(&self) {
        let profiles = self.profiles.lock().clone();
        // Only confirmed ones are kept: one asking is forgotten if Desk
        // restarts before the person answers.
        let kept: Vec<&Profile> = profiles.iter().filter(|p| p.confirmed).collect();
        let write = || -> Result<()> {
            if let Some(dir) = self.file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.file.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&kept)?)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
            }
            std::fs::rename(&tmp, &self.file)?;
            Ok(())
        };
        if let Err(e) = write() {
            warn!(error = %format!("{e:#}"), "keeping Desk's Chrome profiles failed");
        }
    }

    /// Every profile, as Settings → Browser shows them.
    pub fn view(&self) -> Vec<ProfileView> {
        let live = self.live.lock();
        let asking = self.asking.lock();
        self.profiles
            .lock()
            .iter()
            .filter(|p| p.confirmed || asking.contains(&p.id))
            .map(|p| ProfileView {
                id: p.id.clone(),
                label: p.label.clone(),
                version: p.version.clone(),
                state: if p.confirmed { "confirmed" } else { "pending" },
                connected: live.contains_key(&p.id) || asking.contains(&p.id),
                chosen: p.chosen,
            })
            .collect()
    }

    /// Whether the profile Desk uses is connected now.
    pub fn connected(&self) -> bool {
        self.chosen_conn().is_some()
    }

    /// The confirmed profile Desk uses: the one the person chose, else the
    /// one confirmed last.
    fn chosen(&self) -> Option<String> {
        let profiles = self.profiles.lock();
        profiles
            .iter()
            .find(|p| p.confirmed && p.chosen)
            .or_else(|| profiles.iter().rev().find(|p| p.confirmed))
            .map(|p| p.id.clone())
    }

    fn chosen_conn(&self) -> Option<Arc<Conn>> {
        let id = self.chosen()?;
        self.live.lock().get(&id).cloned()
    }

    /// The person confirms profile `id` is theirs. False when no such
    /// profile is asking.
    pub fn confirm(&self, id: &str) -> bool {
        let mut profiles = self.profiles.lock();
        let Some(p) = profiles.iter_mut().find(|p| p.id == id) else {
            return false;
        };
        p.confirmed = true;
        // The one just confirmed is the one Desk uses.
        for q in profiles.iter_mut() {
            q.chosen = q.id == id;
        }
        drop(profiles);
        self.save();
        self.changed.send_modify(|n| *n += 1);
        info!(profile = %id, "a Chrome profile was confirmed for Desk");
        true
    }

    /// The person chooses which confirmed profile Desk uses.
    pub fn choose(&self, id: &str) -> bool {
        let mut profiles = self.profiles.lock();
        if !profiles.iter().any(|p| p.id == id && p.confirmed) {
            return false;
        }
        for p in profiles.iter_mut() {
            p.chosen = p.id == id;
        }
        drop(profiles);
        self.save();
        true
    }

    /// The person forgets a profile (or refuses one asking): it reaches
    /// nothing until confirmed again, and its connection ends now.
    pub fn forget(&self, id: &str) -> bool {
        let mut profiles = self.profiles.lock();
        let before = profiles.len();
        profiles.retain(|p| p.id != id);
        let gone = profiles.len() != before;
        drop(profiles);
        self.asking.lock().remove(id);
        if let Some(conn) = self.live.lock().remove(id) {
            let _ = conn
                .out
                .send(json!({ "type": "refused", "why": "Desk forgot this Chrome." }).to_string());
        }
        self.save();
        self.changed.send_modify(|n| *n += 1);
        gone
    }

    /// Serves one connection until it ends.
    async fn serve(self: Arc<Self>, socket: WebSocket) {
        let (mut sink, mut stream) = socket.split();
        let say = |m: Value| Message::Text(m.to_string().into());
        // Hello: who it says it is.
        let hello = match tokio::time::timeout(HANDSHAKE, next_json(&mut stream)).await {
            Ok(Some(h)) if h["type"] == "hello" => h,
            _ => return,
        };
        if hello["protocol"].as_u64() != Some(PROTOCOL) {
            let _ = sink
                .send(say(json!({ "type": "refused", "why": "This version of Desk's extension doesn't match Desk. Update both." })))
                .await;
            return;
        }
        let Some(key) = hello["key"]
            .as_str()
            .and_then(|k| base64::engine::general_purpose::STANDARD.decode(k).ok())
            .filter(|k| k.len() == 65 && k[0] == 4)
        else {
            return;
        };
        let id = id_of(&key);
        let version: String = hello["version"]
            .as_str()
            .unwrap_or("?")
            .chars()
            .take(32)
            .collect();
        // The challenge, and the proof it holds that key.
        let nonce = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 32]>());
        let confirmed = self
            .profiles
            .lock()
            .iter()
            .any(|p| p.id == id && p.confirmed);
        let state = if confirmed { "confirmed" } else { "pending" };
        if sink
            .send(say(
                json!({ "type": "challenge", "nonce": nonce, "state": state }),
            ))
            .await
            .is_err()
        {
            return;
        }
        let proof = match tokio::time::timeout(HANDSHAKE, next_json(&mut stream)).await {
            Ok(Some(p)) if p["type"] == "proof" => p,
            _ => return,
        };
        if !verify(
            &key,
            &nonce,
            proof["signature"].as_str().unwrap_or_default(),
        ) {
            warn!(profile = %id, "a Chrome connection's proof didn't verify");
            let _ = sink
                .send(say(
                    json!({ "type": "refused", "why": "This Chrome couldn't prove who it is." }),
                ))
                .await;
            return;
        }
        // A second live connection for this profile is refused.
        if self.live.lock().contains_key(&id) || self.asking.lock().contains(&id) {
            warn!(profile = %id, "a second connection from a Chrome profile was refused");
            let _ = sink
                .send(say(json!({ "type": "refused", "why": "Another connection from this Chrome is already Desk's." })))
                .await;
            return;
        }
        {
            let mut profiles = self.profiles.lock();
            match profiles.iter_mut().find(|p| p.id == id) {
                Some(p) => p.version = version.clone(),
                None => profiles.push(Profile {
                    id: id.clone(),
                    key: hello["key"].as_str().unwrap_or_default().to_string(),
                    label: label_of(&key),
                    version: version.clone(),
                    confirmed: false,
                    chosen: false,
                }),
            }
        }
        // Waiting for the person, where they haven't confirmed it yet.
        if !confirmed {
            self.asking.lock().insert(id.clone());
            let _ = sink
                .send(say(json!({ "type": "waiting", "why": "Confirm this Chrome in Desk: Settings → Browser. Check the code there matches this one." })))
                .await;
            let mut changed = self.changed.subscribe();
            let ok = loop {
                if self
                    .profiles
                    .lock()
                    .iter()
                    .any(|p| p.id == id && p.confirmed)
                {
                    break true;
                }
                if !self.profiles.lock().iter().any(|p| p.id == id) {
                    break false;
                }
                tokio::select! {
                    c = changed.changed() => if c.is_err() { break false },
                    m = stream.next() => match m {
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break false,
                        _ => {}
                    },
                }
            };
            self.asking.lock().remove(&id);
            if !ok {
                let _ = sink
                    .send(say(json!({ "type": "refused", "why": "Desk wasn't told this Chrome is yours." })))
                    .await;
                self.profiles.lock().retain(|p| p.id != id || p.confirmed);
                return;
            }
        }
        // Ready: calls may come.
        let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
        let conn = Arc::new(Conn {
            out,
            waiting: Default::default(),
        });
        self.live.lock().insert(id.clone(), Arc::clone(&conn));
        info!(profile = %id, %version, "Desk's Chrome extension connected");
        let _ = sink.send(say(json!({ "type": "ready" }))).await;
        let mut status_every = tokio::time::interval(Duration::from_secs(2));
        let mut last_status = String::new();
        loop {
            tokio::select! {
                out = outgoing.recv() => match out {
                    Some(text) => if sink.send(Message::Text(text.into())).await.is_err() { break },
                    None => break,
                },
                _ = status_every.tick() => {
                    let status = status_now().await.to_string();
                    if status != last_status {
                        if sink.send(Message::Text(status.clone().into())).await.is_err() { break }
                        last_status = status;
                    }
                }
                m = stream.next() => match m {
                    Some(Ok(Message::Text(t))) => self.take(&conn, &t).await,
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                },
            }
        }
        // Gone: its calls fail now, and Desk's tabs there are closed.
        let mut live = self.live.lock();
        if live.get(&id).is_some_and(|c| Arc::ptr_eq(c, &conn)) {
            live.remove(&id);
        }
        drop(live);
        for (_, tx) in conn.waiting.lock().drain() {
            let _ = tx.send(Err(browser::NOT_CONNECTED.to_string()));
        }
        info!(profile = %id, "Desk's Chrome extension disconnected");
    }

    /// What the extension said: a call's result, or something the person
    /// did in Chrome.
    async fn take(&self, conn: &Conn, text: &str) {
        let Ok(m) = serde_json::from_str::<Value>(text) else {
            return;
        };
        match m["type"].as_str() {
            Some("result") => {
                let Some(id) = m["id"].as_u64() else { return };
                let Some(tx) = conn.waiting.lock().remove(&id) else {
                    return;
                };
                let answer = match m["ok"].as_bool() {
                    Some(true) => Ok(m["value"].clone()),
                    _ => Err(m["error"]
                        .as_str()
                        .unwrap_or("Chrome didn't say what went wrong.")
                        .chars()
                        .take(500)
                        .collect()),
                };
                let _ = tx.send(answer);
            }
            Some("event") => match m["event"].as_str() {
                Some("tab-closed") => {
                    if let Some(session) = m["session"].as_str() {
                        browser::tab_closed(session).await;
                    }
                }
                Some("stopped") => crate::desk_browser::stop_everything().await,
                _ => {}
            },
            _ => {}
        }
    }
}

/// What Desk is doing now, for the extension's popup.
async fn status_now() -> Value {
    let live = browser::live().await;
    let activity = live.first().map(|l| {
        json!({ "agent": l.agent, "url": workforce_common::redact::redact_secrets(&l.url), "host": l.host })
    });
    json!({ "type": "status", "activity": activity })
}

async fn next_json(stream: &mut futures::stream::SplitStream<WebSocket>) -> Option<Value> {
    loop {
        match stream.next().await? {
            Ok(Message::Text(t)) => return serde_json::from_str(&t).ok(),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

/// How long a call may take: a wait as long as it asks, and a margin.
fn patience(cmd: &str, args: &Value) -> Duration {
    match cmd {
        "wait" => Duration::from_millis(
            args["timeout_ms"].as_u64().unwrap_or(30_000).min(120_000) + 10_000,
        ),
        "navigate" => Duration::from_secs(45),
        _ => Duration::from_secs(30),
    }
}

#[async_trait::async_trait]
impl ExtensionLink for Chrome {
    async fn call(&self, cmd: &str, args: Value) -> Result<Value> {
        let conn = self
            .chosen_conn()
            .ok_or_else(|| eyre!(browser::NOT_CONNECTED))?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut call = args.clone();
        let Value::Object(map) = &mut call else {
            return Err(eyre!("a call's arguments are an object"));
        };
        map.insert("type".into(), json!("call"));
        map.insert("id".into(), json!(id));
        map.insert("cmd".into(), json!(cmd));
        let (tx, rx) = oneshot::channel();
        conn.waiting.lock().insert(id, tx);
        if conn.out.send(call.to_string()).is_err() {
            conn.waiting.lock().remove(&id);
            return Err(eyre!(browser::NOT_CONNECTED));
        }
        match tokio::time::timeout(patience(cmd, &args), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(eyre!(e)),
            Ok(Err(_)) => Err(eyre!(browser::NOT_CONNECTED)),
            Err(_) => {
                conn.waiting.lock().remove(&id);
                Err(eyre!(
                    "Chrome didn't answer in time, so nothing more was done."
                ))
            }
        }
    }

    fn connected(&self) -> bool {
        Chrome::connected(self)
    }
}

/// Installs the hub's end of Desk's extension, its profiles kept in
/// `file`.
pub fn install(file: PathBuf) -> Arc<Chrome> {
    let chrome = Chrome::new(file);
    if CHROME.set(Arc::clone(&chrome)).is_err() {
        warn!("Desk's Chrome link was installed twice; the first stands");
        return Arc::clone(CHROME.get().expect("installed"));
    }
    if !browser::install_link(Arc::clone(&chrome) as Arc<dyn ExtensionLink>) {
        warn!("a browser link was installed already");
    }
    chrome
}

/// `GET /ws/desk/chrome`: only the person whose Desk this is, by Desk's
/// device token in the header.
pub(crate) async fn handler(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    state: Arc<GatewayState>,
) -> Response {
    let token = ws_token(&headers, None);
    let caller = match authenticate_ws(&state, &token).await {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    if !crate::desk_floor::holds_the_hub(
        state.identity_store.as_ref(),
        true,
        caller.identity_id.as_deref(),
    ) {
        return (
            axum::http::StatusCode::FORBIDDEN,
            axum::Json(
                json!({ "error": "Only the person whose Desk this is connects Chrome to it" }),
            ),
        )
            .into_response();
    }
    let Some(chrome) = installed().cloned() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Desk's browser isn't running",
        )
            .into_response();
    };
    ws.max_message_size(48 * 1024 * 1024)
        .on_upgrade(move |socket| chrome.serve(socket))
        .into_response()
}

#[cfg(test)]
mod tests;

//! The engine's side of a Desk boss link (remote bosses, RB5b): the links
//! this engine holds, its own key, and its uplink to each boss hub.
//!
//! **The links.** Desk's native side hands this engine each link it mints
//! on a boss hub, `PUT /api/desk/boss-links/{group}`: the hub's address and
//! the name it gave itself at mint (its canonical `https` origin), the link's
//! id, its token, the engine and the agent it was made for. The token is
//! kept in this hub's vault at `desk/links/bosses/<link>`, which no agent's
//! vault tools can read, list or write (`AgentReadDenial::DeskLink`), and is
//! never answered back, logged or shown.
//!
//! **The key.** `GET /api/desk/engine-key` answers this engine's Ed25519
//! public key, for Desk to register at mint, making the keypair the first
//! time. The private half is kept at `desk/links/engine-key` in the vault
//! (its file 0600, its value encrypted), and never leaves the engine.
//!
//! **The uplink.** [`start`] keeps one socket open per boss hub this engine
//! holds live links for, carrying all of them:
//!
//! - `https` alone, to the address fixed at link time, as `wss`; no proxy
//!   and no redirect is followed (the socket library follows none);
//! - every link signs this hub's challenge for the hub it dials:
//!   `canonical_hub` of that address, never the name the challenge says;
//! - a call is taken only for a link the hub accepted, with a `seq` above
//!   the last and a `sentAt` within 120 s of this engine's clock, within its
//!   link's calls a minute and starts an hour; then handed to
//!   [`CrewHub::serve_crew`] with the link's id and the call's, which decides
//!   it under this engine's grant for the group, and records it once;
//! - after `ready` it says the asks it has open (none until asks, RB7) and
//!   what happened in each link's group while it was away (`catch_up`);
//! - it reconnects from 1 s, doubling to 60 s, with jitter. A `401` marks
//!   the link it opened with revoked here, and no socket is opened for it
//!   again.
//!
//! Every route here is the hub's own token's alone, from the
//! `Authorization` header alone, as grants are: Desk's native side holds it,
//! and no page, agent or session can ask them.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, put};
use axum::{Json, Router};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::settings::{DeskBossesConfig, VaultActor};
use workforce_common::vault_scope::{desk_boss_link_path, DESK_BOSS_LINKS, DESK_ENGINE_KEY};
use workforce_desk::grant::BossRef;
use zeroize::Zeroizing;

use crate::boss_links::engine_key::{canonical_hub, Challenge, EngineKey, StoredEngineKey};
use crate::boss_links::uplink::{PROTOCOL, PROTOCOL_HEADER};
use crate::desk_crew::served::Offered;
use crate::desk_crew::served::{ServedCall, SERVED_ACT, SERVED_READ};
use crate::desk_crew::CrewHub;
use crate::gateway::GatewayState;
use crate::vault::Vault;

/// The actor every write here is audited as.
const ACTOR: VaultActor<'static> = VaultActor::System("desk_boss_links");
/// How far a call's `sentAt` may be from this engine's clock.
pub const SKEW: chrono::Duration = chrono::Duration::seconds(120);
/// How often the links are read again, for ones added or ended.
const LOOK_EVERY: Duration = Duration::from_secs(15);
/// How long a boss hub may take to accept a socket.
const CONNECT_WITHIN: Duration = Duration::from_secs(15);
/// How long a socket may stay silent: three of the hub's 20 s pings.
pub const QUIET_WITHIN: Duration = Duration::from_secs(60);
/// The largest result sent up.
const MAX_RESULT: usize = 512 * 1024;

/// A link this engine holds, as kept.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeldLink {
    pub group: String,
    /// The boss hub's own name for itself in a grant (`driftwerk`).
    pub hub: String,
    /// Its address, as dialed.
    pub hub_url: String,
    pub link_id: Uuid,
    pub token_id: Uuid,
    token: String,
    pub expires_at: DateTime<Utc>,
    pub engine_id: String,
    pub agent_id: String,
    #[serde(default)]
    pub revoked: bool,
}

impl std::fmt::Debug for HeldLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldLink")
            .field("group", &self.group)
            .field("hub", &self.hub)
            .field("link_id", &self.link_id)
            .finish_non_exhaustive()
    }
}

impl HeldLink {
    fn live(&self, now: DateTime<Utc>) -> bool {
        !self.revoked && now < self.expires_at
    }

    /// The boss the link is for, as a grant names it.
    pub fn boss(&self) -> BossRef {
        BossRef::Remote {
            hub: self.hub.clone(),
            agent: self.agent_id.clone(),
        }
    }
}

/// Every link this engine holds.
pub fn held_links(vault: &Vault) -> Vec<HeldLink> {
    vault
        .list(Some(DESK_BOSS_LINKS))
        .iter()
        .filter_map(|p| vault.get(p))
        .filter_map(|v| serde_json::from_str::<HeldLink>(&v).ok())
        .collect()
}

fn keep(vault: &Vault, link: &HeldLink) -> eyre::Result<()> {
    let value = Zeroizing::new(serde_json::to_string(link)?);
    vault.set_with_actor(
        &desk_boss_link_path(&link.link_id.to_string()),
        &value,
        ACTOR,
    )
}

/// This engine's key: the one kept, or a new one, kept now.
pub fn engine_key(vault: &Vault) -> eyre::Result<EngineKey> {
    // One maker at a time: two first uplinks never make two keys.
    static MAKING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = MAKING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(kept) = vault.get(DESK_ENGINE_KEY) {
        let bytes = Zeroizing::new(
            base64::engine::general_purpose::STANDARD
                .decode(kept.trim())
                .map_err(|_| eyre::eyre!("the engine key kept here can't be read"))?,
        );
        return EngineKey::load(&StoredEngineKey::from_pkcs8(bytes.to_vec()))
            .map_err(|e| eyre::eyre!("{e}"));
    }
    let (key, stored) = EngineKey::generate().map_err(|e| eyre::eyre!("{e}"))?;
    let text = Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(stored.pkcs8()));
    vault.set_with_actor(DESK_ENGINE_KEY, &text, ACTOR)?;
    Ok(key)
}

// ── Routes ──────────────────────────────────────────────────────────────

struct Api {
    state: Arc<GatewayState>,
}

type S = State<Arc<Api>>;

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

fn not_native(s: &Api, h: &HeaderMap, q: &TokenQuery) -> Option<Response> {
    if q.token.is_some() {
        return Some(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "boss links take the hub's token in the Authorization header only"})),
            )
                .into_response(),
        );
    }
    let ours = h
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|given| {
            !s.state.auth_token.is_empty()
                && crate::auth::constant_time_token_eq(given, &s.state.auth_token)
        });
    (!ours).then(|| {
        (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "only Desk itself holds boss links, with the hub's own token"})),
        )
            .into_response()
    })
}

macro_rules! native {
    ($s:expr, $h:expr, $q:expr) => {
        if let Some(r) = not_native(&$s, &$h, &$q) {
            return r;
        }
    };
}

fn bad(why: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": why}))).into_response()
}

fn no_vault() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "this engine keeps no vault, so it holds no boss link"})),
    )
        .into_response()
}

/// What Desk hands over.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PutLink {
    hub: String,
    hub_url: String,
    /// The name the boss hub gave itself at mint: must be the address's.
    canonical_hub: String,
    link_id: Uuid,
    token_id: Uuid,
    token: String,
    expires_at: DateTime<Utc>,
    engine_id: String,
    agent_id: String,
}

/// Whether `url`'s host isn't this computer, a private network or a link's
/// own address, written as one.
pub fn public_address(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    match u.host() {
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d != "localhost" && !d.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(ip)) => public_v4(ip),
        Some(url::Host::Ipv6(ip)) => {
            let seg = ip.segments();
            let o = ip.octets();
            let v4 = |a: usize| std::net::Ipv4Addr::new(o[a], o[a + 1], o[a + 2], o[a + 3]);
            // NAT64 (64:ff9b::/96) and 6to4 (2002::/16) carry an IPv4
            // address: public only when it is.
            let nat64 = seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0];
            let six_to_four = seg[0] == 0x2002;
            !(ip.is_loopback()
                || ip.is_unspecified()
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80
                || ip.to_ipv4_mapped().is_some()
                || (nat64 && !public_v4(v4(12)))
                || (six_to_four && !public_v4(v4(2))))
        }
        None => false,
    }
}

/// Whether `ip` is an address on the public internet, as far as its range
/// says.
fn public_v4(ip: std::net::Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || a == 0
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..20).contains(&b))
        || a >= 240)
}

fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

async fn put_link(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(group): Path<String>,
    body: Bytes,
) -> Response {
    native!(s, h, q);
    let Some(vault) = s.state.vault.as_ref() else {
        return no_vault();
    };
    // One handover at a time: its checks against the links held and its
    // write are one step.
    static HANDING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = HANDING.lock().await;
    let Ok(p) = serde_json::from_slice::<PutLink>(&body) else {
        return bad("that isn't a boss link");
    };
    if !workforce_desk::grant::group_id_ok(&group) {
        return bad("that can't be a group");
    }
    // Only an `https` hub, by the name it gave itself for that address.
    if canonical_hub(&p.hub_url).as_deref() != Some(p.canonical_hub.as_str()) {
        return bad("a boss hub is linked at its https address, by the name it gave for it");
    }
    if !name_ok(&p.hub) || !name_ok(&p.engine_id) || !name_ok(&p.agent_id) {
        return bad("that hub, engine or agent can't be named so");
    }
    if !public_address(&p.hub_url) {
        return bad("a boss hub is linked at a public address, never this computer's or a private network's");
    }
    // A grant names its boss by hub and agent: one hub name is one hub here,
    // and one hub has one name.
    let clash = held_links(vault).into_iter().any(|l| {
        l.link_id != p.link_id
            && (l.hub == p.hub)
                != (canonical_hub(&l.hub_url).as_deref() == Some(p.canonical_hub.as_str()))
    });
    if clash {
        return bad("another boss hub here goes by that name, or this hub by another");
    }
    let token_ok = p
        .token
        .starts_with(workforce_identity::remote_mcp_token::DESK_BOSS_TOKEN_PREFIX)
        && p.token.len() < 200
        && p.token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !token_ok {
        return bad("that isn't a boss link token");
    }
    let link = HeldLink {
        group,
        hub: p.hub,
        hub_url: p.hub_url,
        link_id: p.link_id,
        token_id: p.token_id,
        token: p.token,
        expires_at: p.expires_at,
        engine_id: p.engine_id,
        agent_id: p.agent_id,
        revoked: false,
    };
    match keep(vault, &link) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "a boss link wasn't kept");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "the boss link couldn't be kept"})),
            )
                .into_response()
        }
    }
}

/// The links held here: never a token.
async fn list(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    native!(s, h, q);
    let Some(vault) = s.state.vault.as_ref() else {
        return no_vault();
    };
    let links: Vec<Value> = held_links(vault)
        .iter()
        .map(|l| {
            json!({"group": l.group, "hub": l.hub, "hubUrl": l.hub_url, "linkId": l.link_id,
                   "expiresAt": l.expires_at, "revoked": l.revoked, "agentId": l.agent_id})
        })
        .collect();
    Json(json!({ "links": links })).into_response()
}

async fn forget(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path((_group, link)): Path<(String, Uuid)>,
) -> Response {
    native!(s, h, q);
    let Some(vault) = s.state.vault.as_ref() else {
        return no_vault();
    };
    match vault.delete_with_actor(&desk_boss_link_path(&link.to_string()), ACTOR) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// This engine's public key, made the first time.
async fn public_key(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    native!(s, h, q);
    let Some(vault) = s.state.vault.as_ref() else {
        return no_vault();
    };
    match engine_key(vault) {
        Ok(k) => Json(json!({ "publicKey": k.public_key().to_text() })).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "this engine's key couldn't be made or read");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub(crate) fn routes(state: Arc<GatewayState>) -> Router {
    Router::new()
        .route("/api/desk/boss-links", get(list))
        .route("/api/desk/boss-links/{group}", put(put_link))
        .route("/api/desk/boss-links/{group}/{link}", delete(forget))
        .route("/api/desk/engine-key", get(public_key))
        .with_state(Arc::new(Api { state }))
}

// ── The uplink ──────────────────────────────────────────────────────────

/// The crew tools this engine serves a boss.
fn tools() -> Vec<&'static str> {
    SERVED_READ
        .iter()
        .chain(SERVED_ACT.iter())
        .copied()
        .collect()
}

/// Calls and starts each link made lately, for its limits.
#[derive(Default)]
pub struct Rates {
    calls: HashMap<Uuid, VecDeque<Instant>>,
    starts: HashMap<Uuid, VecDeque<Instant>>,
}

impl Rates {
    /// Forgets what links no longer held made.
    fn prune(&mut self, held: &std::collections::HashSet<Uuid>) {
        self.calls.retain(|id, _| held.contains(id));
        self.starts.retain(|id, _| held.contains(id));
    }

    /// Whether link `link` may make one more call of `tool` now, counting it.
    fn take(&mut self, link: Uuid, tool: &str, limits: &DeskBossesConfig) -> Result<(), String> {
        let now = Instant::now();
        let calls = self.calls.entry(link).or_default();
        calls.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        if calls.len() >= limits.calls_per_minute as usize {
            return Err(format!(
                "This engine takes at most {} calls a minute from a boss.",
                limits.calls_per_minute
            ));
        }
        if tool == "crew.start_session" {
            let starts = self.starts.entry(link).or_default();
            starts.retain(|t| now.duration_since(*t) < Duration::from_secs(3600));
            if starts.len() >= limits.starts_per_hour as usize {
                return Err(format!(
                    "This engine lets a boss start at most {} sessions an hour.",
                    limits.starts_per_hour
                ));
            }
            starts.push_back(now);
        }
        calls.push_back(now);
        Ok(())
    }
}

/// Why a call is refused before it is decided, or none.
pub fn call_refused(
    seq: u64,
    last_seq: u64,
    sent_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<&'static str> {
    if seq <= last_seq {
        return Some("That call came out of order, so this engine didn't take it.");
    }
    match sent_at {
        Some(at) if (now - at).abs() <= SKEW => None,
        _ => Some("That call's time is too far from this engine's clock, so it wasn't taken."),
    }
}

/// What a connection ended in.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// The hub refused the link it opened with: it is revoked here.
    Revoked,
    /// Anything else: tried again later.
    Dropped(String),
}

/// The uplink's settings.
#[derive(Clone)]
pub struct Uplink {
    pub state: Arc<GatewayState>,
    pub crew: Arc<CrewHub>,
    pub limits: DeskBossesConfig,
    /// Where to dial instead of the hub's address: tests alone, which can't
    /// serve `https` here. What is signed is still the hub's own name.
    #[cfg(test)]
    pub dial: Option<String>,
    /// Each link's calls and starts lately: kept across its sockets.
    pub rates: Arc<parking_lot::Mutex<Rates>>,
}

impl Uplink {
    fn dial_url(&self, hub_url: &str) -> Option<String> {
        #[cfg(test)]
        if let Some(d) = &self.dial {
            return Some(format!("{}/ws/desk-boss", d.trim_end_matches('/')));
        }
        canonical_hub(hub_url)?;
        let rest = hub_url.trim().strip_prefix("https://")?;
        Some(format!("wss://{}/ws/desk-boss", rest.trim_end_matches('/')))
    }

    /// One connection to the boss hub at `hub_url`, carrying `links`, until
    /// it ends. `since` is when the last one ended, for `catch_up`.
    pub async fn connect_once(
        &self,
        hub_url: &str,
        links: &[HeldLink],
        since: Option<DateTime<Utc>>,
    ) -> Ended {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::Message as Ws;
        let Some(first) = links.first() else {
            return Ended::Dropped("no links".into());
        };
        // It signs the name of the hub it dials, whatever the hub says.
        let Some(hub) = canonical_hub(hub_url) else {
            return Ended::Dropped("not an https hub".into());
        };
        let Some(url) = self.dial_url(hub_url) else {
            return Ended::Dropped("not an https hub".into());
        };
        let Some(vault) = self.state.vault.as_ref() else {
            return Ended::Dropped("no vault".into());
        };
        let key = match engine_key(vault) {
            Ok(k) => k,
            Err(e) => return Ended::Dropped(e.to_string()),
        };
        let Ok(mut req) = url.as_str().into_client_request() else {
            return Ended::Dropped("bad address".into());
        };
        let (Ok(bearer), Ok(protocol)) = (
            format!("Bearer {}", first.token).parse(),
            PROTOCOL.to_string().parse(),
        ) else {
            return Ended::Dropped("bad token".into());
        };
        req.headers_mut().insert("authorization", bearer);
        req.headers_mut().insert(PROTOCOL_HEADER, protocol);
        let connected =
            tokio::time::timeout(CONNECT_WITHIN, tokio_tungstenite::connect_async(req)).await;
        let (ws, _) = match connected {
            Ok(Ok(c)) => c,
            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(r))) if r.status() == 401 => {
                return Ended::Revoked
            }
            Ok(Err(e)) => return Ended::Dropped(e.to_string()),
            Err(_) => return Ended::Dropped("the hub didn't answer in time".into()),
        };
        let (mut sink, mut stream) = ws.split();
        let read = |m: Option<Result<Ws, _>>| -> Option<Value> {
            match m {
                Some(Ok(Ws::Text(t))) => serde_json::from_str(&t).ok(),
                _ => None,
            }
        };
        let challenge = match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
            Ok(m) => read(m),
            Err(_) => None,
        };
        let Some(challenge) = challenge.filter(|c| c["type"] == "challenge") else {
            return Ended::Dropped("no challenge".into());
        };
        let Some(nonce) = challenge["nonce"]
            .as_str()
            .and_then(|n| {
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(n)
                    .ok()
            })
            .and_then(|n| <[u8; 32]>::try_from(n).ok())
        else {
            return Ended::Dropped("a challenge this engine can't read".into());
        };
        let signed: Vec<Value> = links
            .iter()
            .filter_map(|l| {
                let sig = key.sign_challenge(Challenge {
                    nonce: &nonce,
                    token_id: l.token_id,
                    hub: &hub,
                    engine_id: &l.engine_id,
                })?;
                Some(json!({
                    "token": l.token,
                    "signature": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig),
                }))
            })
            .collect();
        let hello = json!({
            "type": "hello",
            "protocol": PROTOCOL,
            "engine": {"id": first.engine_id, "name": first.engine_id,
                       "version": env!("CARGO_PKG_VERSION")},
            "links": signed,
            "tools": tools(),
        });
        if sink.send(Ws::Text(hello.to_string().into())).await.is_err() {
            return Ended::Dropped("closed at hello".into());
        }
        let ready = match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
            Ok(m) => read(m),
            Err(_) => None,
        };
        let Some(ready) = ready.filter(|r| r["type"] == "ready") else {
            return Ended::Dropped("not ready".into());
        };
        let accepted: BTreeMap<Uuid, &HeldLink> = ready["links"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["linkId"].as_str()?.parse().ok())
            .filter_map(|id: Uuid| links.iter().find(|l| l.link_id == id).map(|l| (id, l)))
            .collect();
        // Each link's person, as the boss hub says: who may answer its
        // asks. A hub that doesn't say is sent none.
        let persons: HashMap<Uuid, String> = ready["links"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| {
                let id: Uuid = l["linkId"].as_str()?.parse().ok()?;
                let person = l["identityId"].as_str().filter(|p| !p.is_empty())?;
                accepted.contains_key(&id).then(|| (id, person.to_string()))
            })
            .collect();
        let asks_taken = ready["features"]
            .as_array()
            .is_some_and(|f| f.iter().any(|x| x == "asks"));
        // A hub that decides its boss's requests on its own model says so.
        let decide_taken = ready["features"]
            .as_array()
            .is_some_and(|f| f.iter().any(|x| x == "decide"));
        // What it asks open goes up again with the first look, and what
        // happened in each group while it was away now.
        let _ = sink
            .send(Ws::Text(
                json!({"type": "asks_open", "asks": []}).to_string().into(),
            ))
            .await;
        let catch_up = self.catch_up(&accepted, since).await;
        let _ = sink.send(Ws::Text(catch_up.to_string().into())).await;
        // Results go out through one writer, so calls can be decided side
        // by side.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(64);
        let writer = tokio::spawn(async move {
            while let Some(v) = rx.recv().await {
                let sent = tokio::time::timeout(
                    Duration::from_secs(10),
                    sink.send(Ws::Text(v.to_string().into())),
                )
                .await;
                if !matches!(sent, Ok(Ok(()))) {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        // What happens in each accepted link's group wakes its boss.
        let events = tokio::spawn(send_events(
            Arc::clone(&self.crew),
            accepted
                .values()
                .map(|l| (l.link_id, l.group.clone(), l.boss()))
                .collect(),
            tx.clone(),
        ));
        // What its sessions ask the person goes up each link whose person
        // the hub named, when the hub takes asks.
        let asks = tokio::spawn(send_asks(
            Arc::clone(&self.crew),
            accepted
                .values()
                .filter(|l| asks_taken && persons.contains_key(&l.link_id))
                .map(|l| (l.link_id, l.group.clone(), l.boss()))
                .collect(),
            tx.clone(),
        ));
        // Where each accepted link's boss's decisions go up, while this
        // socket is up, when the hub takes them.
        let decide_up = self.crew.decide_up(
            accepted
                .values()
                .filter(|_| decide_taken)
                .map(|l| (l.link_id, l.boss().party_id(), l.group.clone()))
                .collect(),
            tx.clone(),
        );
        // At most a few answers are worked on at once; the rest are refused
        // to be sent again.
        let deciding = Arc::new(tokio::sync::Semaphore::new(DECIDING_AT_ONCE));
        let mut last_seq = 0u64;
        let rates = Arc::clone(&self.rates);
        let ended = loop {
            // The hub pings every 20 s: silence three times that long is a
            // socket gone quiet, and it is opened again.
            let msg = match tokio::time::timeout(QUIET_WITHIN, stream.next()).await {
                Ok(Some(m)) => m,
                Ok(None) => break Ended::Dropped("closed".into()),
                Err(_) => break Ended::Dropped("quiet".into()),
            };
            let frame = match msg {
                Ok(Ws::Text(t)) => match serde_json::from_str::<Value>(&t) {
                    Ok(v) => v,
                    Err(_) => continue,
                },
                Ok(Ws::Close(_)) | Err(_) => break Ended::Dropped("closed".into()),
                Ok(_) => continue,
            };
            match frame["type"].as_str() {
                Some("ping") => {
                    let _ = tx.try_send(json!({"type": "pong"}));
                }
                Some("call") => {
                    let call_id = frame["callId"].as_str().unwrap_or_default().to_string();
                    let seq = frame["seq"].as_u64().unwrap_or(0);
                    let sent_at = frame["sentAt"]
                        .as_str()
                        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                        .map(|t| t.with_timezone(&Utc));
                    let refuse = |why: &str| {
                        json!({"type": "result", "callId": call_id, "status": "refused",
                               "body": {"error": why}})
                    };
                    if let Some(why) = call_refused(seq, last_seq, sent_at, Utc::now()) {
                        tracing::info!(target: "sentinel.audit", seq, "a boss call was refused: {why}");
                        let _ = tx.send(refuse(why)).await;
                        continue;
                    }
                    last_seq = seq;
                    let link = frame["linkId"]
                        .as_str()
                        .and_then(|l| l.parse::<Uuid>().ok())
                        .and_then(|l| accepted.get(&l).copied());
                    let Some(link) = link else {
                        let _ = tx
                            .send(refuse(&workforce_desk::scope::not_ours(None)))
                            .await;
                        continue;
                    };
                    let tool = frame["tool"].as_str().unwrap_or_default().to_string();
                    let mut allowed = rates.lock().take(link.link_id, &tool, &self.limits);
                    // Starts are counted from what this engine recorded too,
                    // so a restart doesn't make room.
                    if allowed.is_ok() && tool == "crew.start_session" {
                        let since = Utc::now() - chrono::Duration::hours(1);
                        let started = self
                            .crew
                            .desk()
                            .store()
                            .boss_calls_count(
                                &link.link_id.to_string(),
                                "crew.start_session",
                                &["ok", "pending"],
                                since,
                            )
                            .await
                            .unwrap_or(i64::MAX);
                        if started >= i64::from(self.limits.starts_per_hour) {
                            allowed = Err(format!(
                                "This engine lets a boss start at most {} sessions an hour.",
                                self.limits.starts_per_hour
                            ));
                        }
                    }
                    if let Err(why) = allowed {
                        let _ = tx.send(refuse(&why)).await;
                        continue;
                    }
                    let (crew, tx, link) = (Arc::clone(&self.crew), tx.clone(), link.clone());
                    let input = frame["input"].clone();
                    tokio::spawn(async move {
                        let served = crew
                            .serve_crew(
                                ServedCall {
                                    tool,
                                    input,
                                    boss: link.boss(),
                                    link_id: Some(link.link_id.to_string()),
                                    call_id: Some(call_id.clone()),
                                    ceiling: None,
                                },
                                &link.group,
                            )
                            .await;
                        let mut result = json!({"type": "result", "callId": call_id,
                                                "status": served.status, "body": served.body});
                        if result.to_string().len() > MAX_RESULT {
                            result = json!({"type": "result", "callId": call_id, "status": "refused",
                                            "body": {"error": "The answer was too large to send."}});
                        }
                        let _ = tx.send(result).await;
                    });
                }
                // An answer to one of this engine's asks: taken only on the
                // link it came on, from its person, once.
                Some("decision") => {
                    let link = frame["linkId"]
                        .as_str()
                        .and_then(|l| l.parse::<Uuid>().ok())
                        .and_then(|l| accepted.get(&l).copied());
                    let Some(link) = link else {
                        let _ = tx
                            .send(json!({"type": "decision_refused", "askId": frame["askId"],
                                         "code": "other_link",
                                         "why": "That answer isn't for this request."}))
                            .await;
                        continue;
                    };
                    let Ok(permit) = Arc::clone(&deciding).try_acquire_owned() else {
                        let _ = tx
                            .send(json!({"type": "decision_refused", "askId": frame["askId"],
                                         "code": "busy", "why": "Busy: answer again."}))
                            .await;
                        continue;
                    };
                    let person = persons.get(&link.link_id).cloned();
                    let (crew, tx, link) = (Arc::clone(&self.crew), tx.clone(), link.clone());
                    let frame = frame.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let back = crew
                            .remote_decision(
                                link.link_id,
                                &link.group,
                                &link.boss(),
                                person.as_deref(),
                                &frame,
                            )
                            .await;
                        let _ = tx.send(back).await;
                    });
                }
                // What its boss's model answered of a decision sent up:
                // taken only for one sent on this socket, on its link.
                Some("decided") => self.crew.decided(&decide_up, &frame),
                Some("state") => {
                    tracing::info!(state = ?frame["state"], "a boss hub said a link's state");
                }
                _ => {}
            }
        };
        events.abort();
        asks.abort();
        drop(decide_up);
        drop(tx);
        let _ = writer.await;
        ended
    }

    /// How much happened in each accepted link's group since `since`.
    async fn catch_up(
        &self,
        links: &BTreeMap<Uuid, &HeldLink>,
        since: Option<DateTime<Utc>>,
    ) -> Value {
        let Some(since) = since else {
            return json!({"type": "catch_up", "links": []});
        };
        let events = self
            .crew
            .desk()
            .store()
            .events_since(since, &[], 500, None)
            .await
            .unwrap_or_default();
        let parts: Vec<Value> = links
            .values()
            .map(|l| {
                let n = events
                    .iter()
                    .filter(|e| e.group.as_deref() == Some(l.group.as_str()))
                    .count();
                json!({"linkId": l.link_id, "since": since, "events": n})
            })
            .collect();
        json!({"type": "catch_up", "links": parts})
    }
}

/// Whether `link`'s group has a grant now whose boss isn't the link's: the
/// home moved the group to another boss, or to none.
pub fn boss_moved(crew: &CrewHub, link: &HeldLink) -> bool {
    crew.desk()
        .grant_live(&link.group)
        .is_ok_and(|g| g.boss.as_ref() != Some(&link.boss()))
}

/// The most requests one link sends its boss a minute: under the boss
/// hub's share of a link's frames ([`crate::boss_links::uplink::EVENTS_PER_MINUTE`]),
/// with room for what happens besides. The rest go with a later look.
const OWED_PER_MINUTE: u32 = 20;

/// How often what the boss owes an answer to is looked at.
const OWED_EVERY: Duration = Duration::from_millis(500);

/// The most `event` frames one link sends an hour: past it, what happens
/// is counted and said with the next one sent.
pub const EVENTS_PER_HOUR: usize = 120;

/// Sends `event` frames up `tx` for what happens in each of `links`' groups
/// ([`CrewHub::served_event`] decides what, and what of it the boss sees),
/// at most [`EVENTS_PER_HOUR`] a link; until the socket ends.
async fn send_events(
    crew: Arc<CrewHub>,
    links: Vec<(Uuid, String, BossRef)>,
    tx: tokio::sync::mpsc::Sender<Value>,
) {
    use tokio::sync::broadcast::error::RecvError;
    // Its bosses are connected while this runs: until the socket ends.
    let _up = crew.remote_up(links.iter().map(|(_, _, b)| b.party_id()).collect());
    let (_, mut rx) = crew.desk().subscribe();
    let mut sent: HashMap<Uuid, VecDeque<Instant>> = HashMap::new();
    let mut skipped: HashMap<Uuid, u64> = HashMap::new();
    // Each link's requests given to its boss, while they are open.
    let mut offered: HashMap<Uuid, HashMap<(Uuid, String), Offered>> = HashMap::new();
    // Each link's requests sent this minute: kept under the boss hub's own
    // share of a link's frames, so none is dropped there.
    let mut owed_sent: HashMap<Uuid, (Instant, u32)> = HashMap::new();
    let mut look = tokio::time::interval(OWED_EVERY);
    look.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // (link, the request it gives, if one, frame)
        let frames: Vec<(Uuid, Option<Uuid>, Value)> = tokio::select! {
            got = rx.recv() => {
                let event = match got {
                    Ok(workforce_desk::model::DeskEvent::Activity { event }) => event,
                    Ok(_) | Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return,
                };
                // Each link wakes its own boss alone.
                links
                    .iter()
                    .filter_map(|(link, group, boss)| {
                        crew.served_event(group, boss, &event).map(|f| (*link, None, f))
                    })
                    .collect()
            }
            _ = look.tick() => links
                .iter()
                .flat_map(|(link, group, boss)| {
                    crew.owed_remote(group, boss, offered.entry(*link).or_default())
                        .into_iter()
                        .map(|(asked, f)| (*link, Some(asked), f))
                })
                .collect(),
        };
        for (link, asked, mut frame) in frames {
            let link = &link;
            // A request not sent is looked at again next time.
            let unsent = |offered: &mut HashMap<Uuid, HashMap<(Uuid, String), Offered>>| {
                if let (Some(asked), Some(o)) = (asked, offered.get_mut(link)) {
                    o.retain(|(id, _), _| *id != asked);
                }
            };
            if asked.is_some() {
                let now = Instant::now();
                let (since, n) = owed_sent.entry(*link).or_insert((now, 0));
                if now.duration_since(*since) >= Duration::from_secs(60) {
                    (*since, *n) = (now, 0);
                }
                if *n >= OWED_PER_MINUTE {
                    unsent(&mut offered);
                    continue;
                }
                *n += 1;
            }
            let recent = sent.entry(*link).or_default();
            while recent
                .front()
                .is_some_and(|at| at.elapsed() > Duration::from_secs(3_600))
            {
                recent.pop_front();
            }
            if recent.len() >= EVENTS_PER_HOUR {
                // A request waits for room; anything else is counted.
                match asked {
                    Some(_) => unsent(&mut offered),
                    None => *skipped.entry(*link).or_default() += 1,
                }
                continue;
            }
            frame["type"] = json!("event");
            frame["linkId"] = json!(link);
            frame["skipped"] = json!(skipped.get(link).copied().unwrap_or(0));
            if tx.try_send(frame).is_ok() {
                recent.push_back(Instant::now());
                skipped.remove(link);
            } else {
                unsent(&mut offered);
            }
        }
    }
}

/// How often what each link's sessions ask the person is looked at.
const ASKS_EVERY: Duration = Duration::from_secs(2);

/// How many answers from a boss hub are worked on at once.
const DECIDING_AT_ONCE: usize = 4;

/// The most frames kept to send again once the socket has room.
const UNSENT_KEPT: usize = 256;

/// Sends `ask` and `ask_closed` frames up `tx` for each of `links`
/// ([`CrewHub::asks_up`]): the first look sends every ask still open again
/// with a new nonce; a frame the socket had no room for is sent again as it
/// was, and only when more wait than are kept do a link's asks go up again
/// with new nonces; until the socket ends.
async fn send_asks(
    crew: Arc<CrewHub>,
    links: Vec<(Uuid, String, BossRef)>,
    tx: tokio::sync::mpsc::Sender<Value>,
) {
    if links.is_empty() {
        return;
    }
    let mut look = tokio::time::interval(ASKS_EVERY);
    look.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut again: HashSet<Uuid> = links.iter().map(|(l, _, _)| *l).collect();
    let mut unsent: VecDeque<(Uuid, Value)> = Default::default();
    loop {
        look.tick().await;
        while let Some((link, f)) = unsent.pop_front() {
            if let Err(e) = tx.try_send(f) {
                let tokio::sync::mpsc::error::TrySendError::Full(f) = e else {
                    return;
                };
                unsent.push_front((link, f));
                break;
            }
        }
        for (link, group, boss) in &links {
            let frames = crew.asks_up(*link, group, boss, again.remove(link)).await;
            for f in frames {
                if !unsent.is_empty() {
                    unsent.push_back((*link, f));
                    continue;
                }
                match tx.try_send(f) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(f)) => {
                        unsent.push_back((*link, f));
                    }
                    Err(_) => return,
                }
            }
        }
        if unsent.len() > UNSENT_KEPT {
            for (link, _) in unsent.drain(..) {
                again.insert(link);
            }
        }
    }
}

/// The wait before the next try after `tries` failed ones: 1 s, doubling to
/// 60 s, with up to a quarter more at random.
pub fn backoff(tries: u32) -> Duration {
    use rand::Rng as _;
    let base = Duration::from_secs(1u64 << tries.min(6)).min(Duration::from_secs(60));
    let jitter = rand::thread_rng().gen_range(0..=base.as_millis() as u64 / 4);
    base + Duration::from_millis(jitter)
}

/// Keeps one uplink per boss hub this engine holds live links for, until
/// `cancel`.
pub fn start(uplink: Uplink, cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut running: HashMap<String, (Vec<Uuid>, tokio::task::JoinHandle<()>)> = HashMap::new();
        loop {
            let now = Utc::now();
            let mut by_hub: BTreeMap<String, Vec<HeldLink>> = BTreeMap::new();
            if let Some(vault) = uplink.state.vault.as_ref() {
                for l in held_links(vault).into_iter().filter(|l| l.live(now)) {
                    // A link whose group has another boss now is the old
                    // boss's: forgotten, never dialed again.
                    if boss_moved(&uplink.crew, &l) {
                        tracing::info!(target: "sentinel.audit", link = %l.link_id, group = %l.group, "a boss link was forgotten: its group has another boss now");
                        let _ = vault
                            .delete_with_actor(&desk_boss_link_path(&l.link_id.to_string()), ACTOR);
                        continue;
                    }
                    by_hub.entry(l.hub_url.clone()).or_default().push(l);
                }
            }
            let held: std::collections::HashSet<Uuid> =
                by_hub.values().flatten().map(|l| l.link_id).collect();
            uplink.rates.lock().prune(&held);
            running.retain(|hub, (ids, task)| {
                let same = by_hub
                    .get(hub)
                    .is_some_and(|ls| ls.iter().map(|l| l.link_id).collect::<Vec<_>>() == *ids);
                if !same || task.is_finished() {
                    task.abort();
                }
                same && !task.is_finished()
            });
            for (hub, links) in by_hub {
                if running.contains_key(&hub) {
                    continue;
                }
                let ids = links.iter().map(|l| l.link_id).collect();
                let (u, c, h) = (uplink.clone(), cancel.clone(), hub.clone());
                let task = tokio::spawn(async move { keep_up(u, h, links, c).await });
                running.insert(hub, (ids, task));
            }
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(LOOK_EVERY) => {}
            }
        }
        for (_, (_, t)) in running {
            t.abort();
        }
    });
}

/// One hub's uplink, reconnecting, until its first link is revoked.
async fn keep_up(uplink: Uplink, hub_url: String, links: Vec<HeldLink>, cancel: CancellationToken) {
    let mut tries = 0u32;
    let mut since = None;
    loop {
        let started = Instant::now();
        let ended = tokio::select! {
            e = uplink.connect_once(&hub_url, &links, since) => e,
            _ = cancel.cancelled() => return,
        };
        since = Some(Utc::now());
        match ended {
            Ended::Revoked => {
                if let (Some(vault), Some(first)) = (uplink.state.vault.as_ref(), links.first()) {
                    let mut gone = first.clone();
                    gone.revoked = true;
                    if let Err(e) = keep(vault, &gone) {
                        tracing::warn!(error = %e, "a revoked boss link wasn't marked so");
                    }
                }
                tracing::info!(hub = %hub_url, "a boss hub no longer accepts this engine's link");
                return;
            }
            Ended::Dropped(why) => {
                tracing::info!(hub = %hub_url, why, "an uplink to a boss hub dropped");
            }
        }
        if started.elapsed() > Duration::from_secs(60) {
            tries = 0;
        }
        let wait = backoff(tries);
        tries = tries.saturating_add(1);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = cancel.cancelled() => return,
        }
    }
}

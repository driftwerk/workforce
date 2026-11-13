//! `GET /ws/desk-boss`: an engine's uplink to this boss hub (remote
//! bosses, RB5a, the boss hub's side). An engine (the person's Mac, or one
//! of their machines) opens one socket here and carries every boss link it
//! holds for this hub on it; this hub sends each link's boss's crew calls
//! down it, and the engine, which decides every one under its own grant,
//! answers them. The engine's side is RB5b.
//!
//! **The handshake**, checked in full before anything is sent down it:
//!
//! 1. The engine connects with `Authorization: Bearer <one link token>`
//!    (never `?token=`) and `Desk-Boss-Protocol: 1`. The token is
//!    authenticated ([`authenticate_link`]) before the upgrade.
//! 2. This hub sends `{"type": "challenge", "protocol", "nonce", "hub",
//!    "serverTime"}`: a fresh nonce per socket, and the name this hub goes by
//!    ([`this_hub`]).
//! 3. Within [`Timing::hello_within`] the engine answers one `hello`:
//!    `{"protocol": 1, "engine": {"id", "name", "version"}, "links":
//!    [{"token", "signature"}], "tools": [...]}`. Each link presents its own
//!    plaintext token, authenticated in full, and its engine key's
//!    signature over this hub's challenge ([`LiveLink::proves`]). Every link
//!    must be the first link's person's and engine's, and the first link
//!    must be among them. A link that fails is dropped and audited, and the
//!    rest stay; with none left, or the first among those dropped, the socket
//!    closes.
//! 4. This hub answers `{"type": "ready", "links", "refused"}`.
//!
//! **After it:** `call` frames go down (`callId`, a `seq` that only rises on
//! the socket, `linkId`, `tool`, `input`, `sentAt`), and `result` frames come
//! up (`callId`, `status`, `body`). A result is taken only for a call
//! outstanding on that link on that socket, once. Every link is checked
//! again in full before each call sent for it and at every ping: a link
//! revoked, a device unpaired, an agent that no longer takes boss work, or
//! `[desk_boss]` turned off stops it at the next frame. One socket per link:
//! a second connect for a link takes it from the first. A call not answered
//! before its socket went is sent again, with the same id, on the link's
//! next socket; the engine never does it twice (`boss_calls`).
//!
//! An engine may also send `decide` up (`linkId`, `decideId`, `prompt`): a
//! request at Act fully for the link's boss to decide on its own model,
//! answered by `decided` (`decideId`, `text` or `error`); see
//! [`super::decide`]. `ready` says `"decide"` among its `features`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::auth::{authenticate_link, this_hub, LinkRefusal, LiveLink};
use super::engine_key::{fresh_nonce, NONCE_LEN};
use crate::gateway::GatewayState;

/// The protocol this hub speaks.
pub const PROTOCOL: u32 = 1;
/// The header an engine names it in.
pub const PROTOCOL_HEADER: &str = "desk-boss-protocol";
/// The largest frame either way.
pub const MAX_FRAME: usize = 1024 * 1024;
/// The most links one socket carries.
pub const MAX_LINKS: usize = 16;
/// The most calls waiting for their results at once, every link's.
const MAX_OUTSTANDING: usize = 256;
/// The most of them one link may have, and one person's links together:
/// one stalled engine can't take every place.
const MAX_OUTSTANDING_PER_LINK: usize = 16;
const MAX_OUTSTANDING_PER_PERSON: usize = 64;
/// The most sockets one link token, and one address, may have open before
/// they prove a link.
const MAX_UNPROVED_PER_TOKEN: usize = 2;
const MAX_UNPROVED_PER_ADDRESS: usize = 8;
/// The most frames this hub can't use before it closes a socket.
const MAX_BAD_FRAMES: u32 = 20;
/// How many checks in a row a link may fail to be checked (the store
/// unreachable) before it is dropped.
const MAX_UNCHECKED: u32 = 3;
/// How long one write to an engine may take before its socket is closed.
const WRITE_WITHIN: Duration = Duration::from_secs(10);
/// The most tool names a link may offer, and the longest.
const MAX_TOOLS: usize = 64;
const MAX_TOOL_NAME: usize = 64;

/// How long things may take.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub hello_within: Duration,
    pub call_within: Duration,
    pub ping_every: Duration,
    /// How long a call no socket has answered is kept to send again.
    pub resend_within: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            hello_within: Duration::from_secs(10),
            call_within: Duration::from_secs(60),
            ping_every: Duration::from_secs(20),
            resend_within: Duration::from_secs(120),
        }
    }
}

/// One link on one socket.
struct OnSocket {
    /// Its plaintext token, kept to check it again in full at each frame.
    token: Zeroizing<String>,
    group_key: String,
    /// The tools its engine offers, `crew.*` alone.
    tools: Vec<String>,
    /// The person and the agent it was minted for, and its engine's name
    /// as registered at mint: what a boss tool binds a call to.
    person: Uuid,
    agent_id: String,
    engine_name: String,
    /// Checks in a row that couldn't be made.
    unchecked: u32,
}

/// One engine's socket.
struct Socket {
    id: Uuid,
    /// The person every link on it is for.
    person: Uuid,
    tx: mpsc::Sender<Message>,
    seq: AtomicU64,
    links: parking_lot::Mutex<HashMap<Uuid, OnSocket>>,
}

impl Socket {
    /// Queues `v` without waiting: a socket whose engine stopped reading
    /// never holds anyone up; what doesn't fit is dropped.
    fn notify(&self, v: Value) -> bool {
        self.tx
            .try_send(Message::Text(v.to_string().into()))
            .is_ok()
    }

    /// Asks the socket to close, without waiting.
    fn close(&self) {
        let _ = self.tx.try_send(Message::Close(None));
    }
}

/// A call waiting for its result.
struct Pending {
    link_id: Uuid,
    person: Uuid,
    /// The socket it was last sent on.
    socket: Uuid,
    tool: String,
    input: Value,
    sent_at: Instant,
    tx: Option<oneshot::Sender<Result<Value, String>>>,
}

/// Every engine's uplink to this hub: which socket carries each link, and
/// the calls waiting for results.
#[derive(Default)]
pub struct Uplinks {
    links: parking_lot::Mutex<HashMap<Uuid, Arc<Socket>>>,
    /// Sockets open but not yet proved, by link token and by address.
    unproved: parking_lot::Mutex<HashMap<String, usize>>,
    pending: parking_lot::Mutex<HashMap<Uuid, Pending>>,
    timing: parking_lot::Mutex<Timing>,
    /// Where the links' wake-ups go ([`super::wake`]), once this hub wakes
    /// bosses.
    wakes: std::sync::OnceLock<tokio::sync::mpsc::Sender<super::wake::Wake>>,
    /// How many of each link's wakes wait on the queue now.
    wakes_waiting: parking_lot::Mutex<HashMap<Uuid, usize>>,
    #[cfg(test)]
    pub(crate) hub: parking_lot::Mutex<Option<String>>,
}

impl std::fmt::Debug for Uplinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Uplinks")
            .field("links", &self.links.lock().len())
            .finish_non_exhaustive()
    }
}

/// What a link's call came to.
pub type CallResult = Result<Value, String>;

/// Why a link checked again didn't hold, and whether that was only this hub
/// not being able to check it now (not the engine's doing).
#[derive(Debug)]
pub struct Rechecked {
    pub why: String,
    pub unavailable: bool,
}

impl Rechecked {
    fn refused(why: impl Into<String>) -> Self {
        Self {
            why: why.into(),
            unavailable: false,
        }
    }
}

impl From<Rechecked> for String {
    fn from(r: Rechecked) -> Self {
        r.why
    }
}

/// The most `event` frames a socket takes for one link a minute: the rest
/// are dropped before any check, and nothing is counted against it.
pub const EVENTS_PER_MINUTE: u32 = 30;

/// The most `ask` frames one link is taken a minute; the rest are dropped
/// before any check (the engine sends an open ask again on its next socket).
pub const ASKS_PER_MINUTE: u32 = 30;

/// Whether `link` is past `most` this minute in `seen`, counting this one.
fn past_share(seen: &mut HashMap<Uuid, (Instant, u32)>, link: Uuid, most: u32) -> bool {
    let now = Instant::now();
    let (since, n) = seen.entry(link).or_insert((now, 0));
    if now.duration_since(*since) >= Duration::from_secs(60) {
        (*since, *n) = (now, 0);
    }
    *n += 1;
    *n > most
}

/// The most wakes of one link waiting to be gathered at once: one link's
/// flood never takes another's place.
pub const WAKES_WAITING_PER_LINK: usize = 8;

/// A place among the unproved sockets, given back when dropped.
struct Unproved {
    uplinks: Arc<Uplinks>,
    keys: Vec<String>,
}

impl Drop for Unproved {
    fn drop(&mut self) {
        let mut u = self.uplinks.unproved.lock();
        for k in &self.keys {
            if let Some(n) = u.get_mut(k) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    u.remove(k);
                }
            }
        }
    }
}

impl Uplinks {
    /// Sends `frame` down whichever socket carries `link` now; false when
    /// none does or its queue is full.
    pub(crate) fn notify_link(&self, link: Uuid, frame: Value) -> bool {
        let socket = self.links.lock().get(&link).cloned();
        socket.is_some_and(|s| s.links.lock().contains_key(&link) && s.notify(frame))
    }

    /// A place for one more socket of token `token_id` from `address` to
    /// prove itself, unless either has as many open as it may.
    fn unproved(
        self: &Arc<Self>,
        token_id: Uuid,
        address: Option<std::net::IpAddr>,
    ) -> Option<Unproved> {
        let mut keys = vec![format!("token:{token_id}")];
        if let Some(a) = address {
            keys.push(format!("address:{a}"));
        }
        let mut u = self.unproved.lock();
        let full = keys.iter().any(|k| {
            let most = match k.starts_with("token:") {
                true => MAX_UNPROVED_PER_TOKEN,
                false => MAX_UNPROVED_PER_ADDRESS,
            };
            u.get(k).copied().unwrap_or(0) >= most
        });
        if full {
            return None;
        }
        for k in &keys {
            *u.entry(k.clone()).or_default() += 1;
        }
        Some(Unproved {
            uplinks: Arc::clone(self),
            keys,
        })
    }

    pub fn set_timing(&self, t: Timing) {
        *self.timing.lock() = t;
    }

    fn timing(&self) -> Timing {
        *self.timing.lock()
    }

    /// The name this hub goes by in a challenge.
    fn hub(&self, state: &GatewayState) -> Option<String> {
        #[cfg(test)]
        if let Some(h) = self.hub.lock().clone() {
            return Some(h);
        }
        this_hub(state)
    }

    /// Whether link `link_id` is on a socket now.
    pub fn connected(&self, link_id: Uuid) -> bool {
        self.links.lock().contains_key(&link_id)
    }

    /// The links on a socket now whose group is `group_key`: what one Desk
    /// group's engines are, for fanning out (RB6).
    pub fn links_of_group(&self, group_key: &str) -> Vec<Uuid> {
        self.links
            .lock()
            .iter()
            .filter(|(id, s)| {
                s.links
                    .lock()
                    .get(id)
                    .is_some_and(|l| l.group_key == group_key)
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// Sends this hub's links' wake-ups to `tx`: once.
    pub fn wake_into(&self, tx: tokio::sync::mpsc::Sender<super::wake::Wake>) {
        let _ = self.wakes.set(tx);
    }

    /// Hands `wake` on, when this hub wakes bosses; never waits. Each link
    /// has [`WAKES_WAITING_PER_LINK`] places: past them its wakes are
    /// dropped, and no other link's are.
    fn wake(&self, wake: super::wake::Wake) {
        let Some(tx) = self.wakes.get() else {
            return;
        };
        let link = wake.link_id;
        {
            let mut waiting = self.wakes_waiting.lock();
            let n = waiting.entry(link).or_default();
            if *n >= WAKES_WAITING_PER_LINK {
                tracing::warn!(%link, "a boss's wake-up was dropped: its link's places are full");
                return;
            }
            *n += 1;
        }
        if tx.try_send(wake).is_err() {
            self.wake_taken(link);
            tracing::warn!(%link, "a boss's wake-up was dropped: the queue is full");
        }
    }

    /// One of `link`'s wakes was taken off the queue.
    pub(crate) fn wake_taken(&self, link: Uuid) {
        let mut waiting = self.wakes_waiting.lock();
        if let Some(n) = waiting.get_mut(&link) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                waiting.remove(&link);
            }
        }
    }

    /// The links on a socket now of the group `group_key` minted for
    /// `person` and `agent`, with each one's engine name as registered at
    /// mint: all a boss tool in that group's conversation may call. Never
    /// another person's, another agent's or another group's.
    pub fn links_for(&self, group_key: &str, person: Uuid, agent: &str) -> Vec<(Uuid, String)> {
        let mut out: Vec<(Uuid, String)> = self
            .links
            .lock()
            .iter()
            .filter_map(|(id, s)| {
                let links = s.links.lock();
                let l = links.get(id)?;
                (l.group_key == group_key && l.person == person && l.agent_id == agent)
                    .then(|| (*id, l.engine_name.clone()))
            })
            .collect();
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    /// The crew tools link `link_id`'s engine offers: names alone, as the
    /// engine said them. Untrusted.
    pub fn tools_of(&self, link_id: Uuid) -> Vec<String> {
        self.links
            .lock()
            .get(&link_id)
            .and_then(|s| s.links.lock().get(&link_id).map(|l| l.tools.clone()))
            .unwrap_or_default()
    }

    /// Takes `link_id` off `socket`, and off the registry while that socket
    /// carries it; closes the socket when it carries none.
    async fn drop_link(&self, socket: &Arc<Socket>, link_id: Uuid, why: &str) {
        let left = {
            let mut links = socket.links.lock();
            links.remove(&link_id);
            links.len()
        };
        {
            let mut reg = self.links.lock();
            if reg.get(&link_id).is_some_and(|s| s.id == socket.id) {
                reg.remove(&link_id);
            }
        }
        socket.notify(json!({"type": "state", "linkId": link_id, "state": "stopped", "why": why}));
        if left == 0 {
            socket.close();
        }
    }

    /// Checks `link_id` on `socket` again in full: the live link, or why
    /// not (and then it is dropped).
    async fn recheck(
        &self,
        state: &GatewayState,
        socket: &Arc<Socket>,
        link_id: Uuid,
    ) -> Result<LiveLink, Rechecked> {
        let token = socket
            .links
            .lock()
            .get(&link_id)
            .map(|l| l.token.clone())
            .ok_or_else(|| Rechecked::refused("That boss link isn't connected."))?;
        match authenticate_link(state, &token) {
            Ok(live) if live.link.link_id == link_id => {
                if let Some(l) = socket.links.lock().get_mut(&link_id) {
                    l.unchecked = 0;
                }
                Ok(live)
            }
            Ok(_) | Err(LinkRefusal::Unauthenticated) => {
                let why = LinkRefusal::Unauthenticated.to_string();
                self.drop_link(socket, link_id, &why).await;
                Err(Rechecked::refused(why))
            }
            // Couldn't check now: not sent; dropped once it has gone on.
            Err(LinkRefusal::Unavailable) => {
                let unchecked = socket.links.lock().get_mut(&link_id).map_or(0, |l| {
                    l.unchecked += 1;
                    l.unchecked
                });
                if unchecked >= MAX_UNCHECKED {
                    self.drop_link(socket, link_id, &LinkRefusal::Unavailable.to_string())
                        .await;
                }
                Err(Rechecked {
                    why: LinkRefusal::Unavailable.to_string(),
                    unavailable: true,
                })
            }
            Err(why) => {
                let why = why.to_string();
                self.drop_link(socket, link_id, &why).await;
                Err(Rechecked::refused(why))
            }
        }
    }

    /// Sends `tool` with `input` to the engine that carries link `link_id`,
    /// and waits for its result: what RB6's boss tools call. The link is
    /// checked again in full first. The result body is the engine's,
    /// untrusted: wrap it before an agent reads it.
    pub async fn call(
        &self,
        state: &GatewayState,
        link_id: Uuid,
        tool: &str,
        input: Value,
    ) -> CallResult {
        let socket = self
            .links
            .lock()
            .get(&link_id)
            .cloned()
            .ok_or_else(|| "That engine isn't connected.".to_string())?;
        self.recheck(state, &socket, link_id).await?;
        let timing = self.timing();
        let call_id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock();
            pending.retain(|_, p| p.sent_at.elapsed() < timing.resend_within);
            let of_link = pending.values().filter(|p| p.link_id == link_id).count();
            let of_person = pending
                .values()
                .filter(|p| p.person == socket.person)
                .count();
            if pending.len() >= MAX_OUTSTANDING
                || of_link >= MAX_OUTSTANDING_PER_LINK
                || of_person >= MAX_OUTSTANDING_PER_PERSON
            {
                return Err("Too many calls are waiting for their engines; try again soon.".into());
            }
            pending.insert(
                call_id,
                Pending {
                    link_id,
                    person: socket.person,
                    socket: socket.id,
                    tool: tool.to_string(),
                    input: input.clone(),
                    sent_at: Instant::now(),
                    tx: Some(tx),
                },
            );
        }
        // Sending it and its answer share the one wait: an engine that
        // stopped reading holds the call no longer than that.
        let frame = Message::Text(
            call_frame(&socket, call_id, link_id, tool, &input)
                .to_string()
                .into(),
        );
        let got = tokio::time::timeout(timing.call_within, async {
            if socket.tx.send(frame).await.is_err() {
                // Its socket went: it is sent again on the link's next one.
                tracing::info!(%link_id, "a boss call waits for its engine to come back");
            }
            rx.await
        })
        .await;
        self.pending.lock().remove(&call_id);
        match got {
            Ok(Ok(result)) => result,
            _ => Err(
                "The engine didn't answer in time; what it did, if anything, is in its activity."
                    .into(),
            ),
        }
    }

    /// A result from `socket` for `call_id`: taken once, only for a call
    /// outstanding on that link on that socket.
    fn take_result(&self, socket: &Socket, call_id: Uuid, result: CallResult) -> bool {
        let mut pending = self.pending.lock();
        let ours = pending
            .get(&call_id)
            .is_some_and(|p| p.socket == socket.id && socket.links.lock().contains_key(&p.link_id));
        if !ours {
            return false;
        }
        match pending.get_mut(&call_id).and_then(|p| p.tx.take()) {
            Some(tx) => {
                pending.remove(&call_id);
                tx.send(result).is_ok()
            }
            None => false,
        }
    }

    /// Sends every call still unanswered for `links` again on `socket`,
    /// each with the id it had.
    async fn resend(&self, socket: &Arc<Socket>, links: &HashSet<Uuid>) {
        let again: Vec<(Uuid, Uuid, String, Value)> = {
            let mut pending = self.pending.lock();
            pending
                .iter_mut()
                .filter(|(_, p)| links.contains(&p.link_id) && p.tx.is_some())
                .map(|(id, p)| {
                    p.socket = socket.id;
                    (*id, p.link_id, p.tool.clone(), p.input.clone())
                })
                .collect()
        };
        for (call_id, link_id, tool, input) in again {
            socket.notify(call_frame(socket, call_id, link_id, &tool, &input));
        }
    }
}

fn call_frame(socket: &Socket, call_id: Uuid, link_id: Uuid, tool: &str, input: &Value) -> Value {
    json!({
        "type": "call",
        "callId": call_id,
        "seq": socket.seq.fetch_add(1, Ordering::SeqCst) + 1,
        "linkId": link_id,
        "tool": tool,
        "input": input,
        "sentAt": chrono::Utc::now(),
    })
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Deserialize)]
pub(crate) struct UplinkQuery {
    token: Option<String>,
}

fn refused(status: StatusCode, why: impl Into<String>) -> Response {
    (status, Json(json!({ "error": why.into() }))).into_response()
}

/// `GET /ws/desk-boss`.
pub(crate) async fn handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Query(q): Query<UplinkQuery>,
    State(state): State<Arc<GatewayState>>,
    extensions: axum::http::Extensions,
) -> Response {
    if q.token.is_some() {
        return refused(
            StatusCode::BAD_REQUEST,
            "An uplink takes its link token in the Authorization header only.",
        );
    }
    let protocol = headers
        .get(PROTOCOL_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim);
    if protocol != Some(&PROTOCOL.to_string()) {
        return refused(
            StatusCode::UPGRADE_REQUIRED,
            format!("This hub speaks Desk-Boss-Protocol {PROTOCOL}."),
        );
    }
    let Some(hub) = state.uplinks.hub(&state) else {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "This hub has no public address, so no boss link can be proved here.",
        );
    };
    let Some(token) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| Zeroizing::new(t.trim().to_string()))
    else {
        return refused(
            StatusCode::UNAUTHORIZED,
            LinkRefusal::Unauthenticated.to_string(),
        );
    };
    let first = match authenticate_link(&state, &token) {
        Ok(live) => live,
        Err(LinkRefusal::Unauthenticated) => {
            return refused(
                StatusCode::UNAUTHORIZED,
                LinkRefusal::Unauthenticated.to_string(),
            )
        }
        Err(LinkRefusal::Unavailable) => {
            return refused(
                StatusCode::SERVICE_UNAVAILABLE,
                LinkRefusal::Unavailable.to_string(),
            )
        }
        Err(why) => return refused(StatusCode::FORBIDDEN, why.to_string()),
    };
    // The client's address as the hub's trusted proxies say it, never a
    // proxy's own.
    let peer = extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .cloned();
    let xff = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
    let address = Some(crate::trusted_proxy::extract_client_ip(
        &peer,
        xff,
        &state.trusted_proxies,
    ));
    let Some(place) = state.uplinks.unproved(first.token.id, address) else {
        audit(
            &state,
            &first,
            "desk_boss.uplink.refused",
            "too many unproved sockets",
        );
        return refused(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many uplinks for this link are still proving themselves; try again shortly.",
        );
    };
    ws.max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |socket| run(socket, state, hub, first, place))
}

/// One link of a `hello`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HelloLink {
    token: String,
    signature: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HelloEngine {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Hello {
    #[serde(rename = "type")]
    kind: String,
    protocol: u32,
    engine: HelloEngine,
    links: Vec<HelloLink>,
    #[serde(default)]
    tools: Vec<String>,
}

/// The `crew.*` tool names of `offered`, capped.
fn crew_tools(offered: &[String]) -> Vec<String> {
    offered
        .iter()
        .filter(|t| {
            t.starts_with("crew.")
                && t.len() <= MAX_TOOL_NAME
                && t[5..]
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c == '.')
        })
        .take(MAX_TOOLS)
        .cloned()
        .collect()
}

fn audit(state: &GatewayState, live: &LiveLink, action: &str, how: &str) {
    super::audit(
        state.telemetry_store.as_ref(),
        &format!("desk-boss:{}", live.link.link_id),
        action,
        &live.token,
        how,
    );
}

async fn run(
    ws: WebSocket,
    state: Arc<GatewayState>,
    hub: String,
    first: LiveLink,
    place: Unproved,
) {
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::channel::<Message>(64);
    // Each write gets its time: an engine that stops reading is closed.
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let close = matches!(m, Message::Close(_));
            match tokio::time::timeout(WRITE_WITHIN, sink.send(m)).await {
                Ok(Ok(())) if !close => {}
                _ => break,
            }
        }
        let _ = tokio::time::timeout(WRITE_WITHIN, sink.close()).await;
    });
    let socket = Arc::new(Socket {
        id: Uuid::new_v4(),
        person: first.person.id,
        tx,
        seq: AtomicU64::new(0),
        links: Default::default(),
    });
    let uplinks = &state.uplinks;
    let timing = uplinks.timing();
    let close = |why: &str| {
        let socket = Arc::clone(&socket);
        let why = why.to_string();
        async move {
            socket.notify(json!({"type": "error", "why": why}));
            socket.close();
        }
    };
    let Ok(nonce) = fresh_nonce() else {
        close("This hub couldn't make a challenge.").await;
        let _ = writer.await;
        return;
    };
    socket.notify(json!({
        "type": "challenge",
        "protocol": PROTOCOL,
        "nonce": b64(&nonce),
        "hub": hub,
        "serverTime": chrono::Utc::now(),
    }));
    let hello = match tokio::time::timeout(timing.hello_within, stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Hello>(&t).ok(),
        _ => None,
    };
    let Some(hello) = hello.filter(|h| h.kind == "hello") else {
        audit(
            &state,
            &first,
            "desk_boss.uplink.refused",
            "no hello in time",
        );
        close("An uplink says hello first, in time.").await;
        let _ = writer.await;
        return;
    };
    let accepted = match handshake(&state, &hub, &nonce, &first, &hello, &socket) {
        Ok(a) => a,
        Err(why) => {
            audit(&state, &first, "desk_boss.uplink.refused", &why);
            close(&why).await;
            let _ = writer.await;
            return;
        }
    };
    // Proved: no longer among the unproved.
    drop(place);
    // One socket per link: this one takes each from any other.
    let mut replaced = Vec::new();
    {
        let mut reg = uplinks.links.lock();
        for (link_id, _) in &accepted {
            if let Some(old) = reg.insert(*link_id, Arc::clone(&socket)) {
                if old.id != socket.id {
                    replaced.push((old, *link_id));
                }
            }
        }
    }
    for (old, link_id) in replaced {
        let left = {
            let mut links = old.links.lock();
            links.remove(&link_id);
            links.len()
        };
        old.notify(
            json!({"type": "state", "linkId": link_id, "state": "replaced",
                          "why": "This link connected from somewhere new."}),
        );
        if left == 0 {
            old.close();
        }
        if let Some((_, live)) = accepted.iter().find(|(id, _)| *id == link_id) {
            audit(&state, live, "desk_boss.uplink.replaced", "replaced");
        }
    }
    let ids: HashSet<Uuid> = accepted.iter().map(|(id, _)| *id).collect();
    uplinks.resend(&socket, &ids).await;
    // The loop: results and pongs up, pings down, every link checked again
    // at each ping.
    let mut ticker = tokio::time::interval(timing.ping_every);
    ticker.tick().await;
    let mut unanswered = 0u32;
    // Each link's events this minute: (since, how many).
    let mut events: HashMap<Uuid, (Instant, u32)> = HashMap::new();
    let mut asks: HashMap<Uuid, (Instant, u32)> = HashMap::new();
    let mut decides: HashMap<Uuid, (Instant, u32)> = HashMap::new();
    let deciding = Arc::new(tokio::sync::Semaphore::new(super::decide::DECIDES_AT_ONCE));
    let mut bad = 0u32;
    loop {
        if bad > MAX_BAD_FRAMES {
            tracing::info!("an uplink sent too many frames this hub can't use: closed");
            break;
        }
        tokio::select! {
            msg = stream.next() => {
                let Some(Ok(msg)) = msg else { break };
                let Message::Text(t) = msg else {
                    if matches!(msg, Message::Close(_)) { break }
                    continue;
                };
                let Ok(frame) = serde_json::from_str::<Value>(&t) else {
                    bad += 1;
                    continue;
                };
                match frame["type"].as_str() {
                    Some("pong") => unanswered = 0,
                    Some("ping") => {
                        if !socket.notify(json!({"type": "pong"})) { break }
                    }
                    Some("result") => {
                        let Some(call_id) = frame["callId"].as_str().and_then(|c| c.parse().ok()) else {
                            bad += 1;
                            continue;
                        };
                        let status = frame["status"].as_str().unwrap_or("refused");
                        let body = frame["body"].clone();
                        let result = match status {
                            "refused" => Err(body.get("error").and_then(Value::as_str)
                                .unwrap_or("The engine refused it.").to_string()),
                            "ok" | "suggested" | "escalated" => Ok(json!({"status": status, "body": body})),
                            _ => Err("The engine's answer couldn't be read.".to_string()),
                        };
                        // A result for a call this hub no longer knows (it
                        // restarted, or the call timed out) is dropped, not
                        // counted against the engine.
                        let _ = uplinks.take_result(&socket, call_id, result);
                    }
                    // A wake-up for the link's boss: bound to the link this
                    // socket proved and still holds, its person, agent and
                    // group the link's, whatever the frame says.
                    Some("event") => {
                        let Some(link) = frame["linkId"].as_str().and_then(|l| l.parse::<Uuid>().ok()) else {
                            bad += 1;
                            continue;
                        };
                        if !socket.links.lock().contains_key(&link) {
                            bad += 1;
                            continue;
                        }
                        // Past its minute's share: dropped before any check.
                        if past_share(&mut events, link, EVENTS_PER_MINUTE) {
                            continue;
                        }
                        let live = match uplinks.recheck(&state, &socket, link).await {
                            Ok(live) => live,
                            // This hub couldn't check: not the engine's doing.
                            Err(r) if r.unavailable => continue,
                            Err(_) => {
                                bad += 1;
                                continue;
                            }
                        };
                        match super::wake::Wake::of(&live, &frame) {
                            Some(wake) => uplinks.wake(wake),
                            None => bad += 1,
                        }
                    }
                    // What a link's sessions ask its person, and what came
                    // of it: bound to the link this socket proved and still
                    // holds, checked again, and kept for the link's person
                    // and agent whatever the frame says.
                    Some(kind @ ("ask" | "ask_closed" | "decision_refused")) => {
                        let Some(link) = frame["linkId"].as_str().and_then(|l| l.parse::<Uuid>().ok()) else {
                            bad += 1;
                            continue;
                        };
                        if !socket.links.lock().contains_key(&link) {
                            bad += 1;
                            continue;
                        }
                        if past_share(&mut asks, link, ASKS_PER_MINUTE) {
                            continue;
                        }
                        let live = match uplinks.recheck(&state, &socket, link).await {
                            Ok(live) => live,
                            Err(r) if r.unavailable => continue,
                            Err(_) => {
                                bad += 1;
                                continue;
                            }
                        };
                        if kind == "ask" {
                            if super::asks::Asked::of(&frame, chrono::Utc::now()).is_none() {
                                bad += 1;
                                continue;
                            }
                            let (state, hub) = (Arc::clone(&state), hub.clone());
                            tokio::spawn(async move {
                                let _ = super::asks::asked(&state, &live, &frame, &hub).await;
                            });
                        } else {
                            let state = Arc::clone(&state);
                            tokio::spawn(async move {
                                super::asks::closed(&state, &live, &frame).await;
                            });
                        }
                    }
                    // A request at Act fully for the link's boss to decide
                    // on its own model ([`super::decide`]): bound to the
                    // link this socket proved and still holds, checked
                    // again, run as the link's agent whatever the frame
                    // says. One past its share, or with no room, is
                    // answered so, and the engine leaves it to the person.
                    Some("decide") => {
                        let Some(link) = frame["linkId"].as_str().and_then(|l| l.parse::<Uuid>().ok()) else {
                            bad += 1;
                            continue;
                        };
                        if !socket.links.lock().contains_key(&link) {
                            bad += 1;
                            continue;
                        }
                        let Some((id, prompt)) = super::decide::asked(&frame) else {
                            bad += 1;
                            continue;
                        };
                        let refuse = |why: &str| {
                            socket.notify(super::decide::answered(link, id, Err(why.to_string())));
                        };
                        if past_share(&mut decides, link, super::decide::DECIDES_PER_MINUTE) {
                            refuse("its hub decides no more this minute");
                            continue;
                        }
                        let Ok(permit) = Arc::clone(&deciding).try_acquire_owned() else {
                            refuse("its hub is deciding as many as it can at once");
                            continue;
                        };
                        let live = match uplinks.recheck(&state, &socket, link).await {
                            Ok(live) => live,
                            Err(r) => {
                                refuse(&r.why);
                                continue;
                            }
                        };
                        let s = Arc::clone(&socket);
                        super::decide::spawn(Arc::clone(&state), live, id, prompt, permit, move |v| {
                            s.notify(v);
                        });
                    }
                    // Frames of later versions (asks_open, catch_up, …):
                    // not counted against it.
                    Some("asks_open" | "catch_up") => {}
                    _ => bad += 1,
                }
            }
            _ = ticker.tick() => {
                unanswered += 1;
                if unanswered > 3 {
                    break;
                }
                let ids: Vec<Uuid> = socket.links.lock().keys().copied().collect();
                for id in ids {
                    let _ = uplinks.recheck(&state, &socket, id).await;
                }
                if socket.links.lock().is_empty() {
                    break;
                }
                if !socket.notify(json!({"type": "ping"})) {
                    break;
                }
            }
        }
    }
    // Gone: its links are on no socket, unless another took them.
    {
        let mut reg = uplinks.links.lock();
        reg.retain(|_, s| s.id != socket.id);
    }
    socket.close();
    drop(socket);
    let _ = writer.await;
}

/// The links `hello` proves, checked as the module says, with each
/// refusal audited; or why the whole socket is refused.
fn handshake(
    state: &GatewayState,
    hub: &str,
    nonce: &[u8; NONCE_LEN],
    first: &LiveLink,
    hello: &Hello,
    socket: &Arc<Socket>,
) -> Result<Vec<(Uuid, LiveLink)>, String> {
    if hello.protocol != PROTOCOL {
        return Err(format!("This hub speaks Desk-Boss-Protocol {PROTOCOL}."));
    }
    if hello.links.is_empty() || hello.links.len() > MAX_LINKS {
        return Err(format!("An uplink carries 1 to {MAX_LINKS} links."));
    }
    if hello.engine.id != first.link.engine_id {
        return Err("That isn't the engine this link was made for.".into());
    }
    let tools = crew_tools(&hello.tools);
    let mut accepted: Vec<(Uuid, LiveLink)> = Vec::new();
    let mut refused: Vec<Value> = Vec::new();
    let mut first_proved = false;
    for (i, l) in hello.links.iter().enumerate() {
        let live = match authenticate_link(state, &l.token) {
            Ok(live) => live,
            Err(why) => {
                refused.push(json!({"index": i, "why": why.to_string()}));
                continue;
            }
        };
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&l.signature)
            .unwrap_or_default();
        let why = if live.person.id != first.person.id {
            Some("That link is another person's.")
        } else if live.link.engine_id != first.link.engine_id {
            Some("That link is another engine's.")
        } else if accepted.iter().any(|(id, _)| *id == live.link.link_id) {
            Some("That link is on this uplink already.")
        } else if !live.proves(hub, nonce, &signature) {
            Some("That link's engine key didn't prove it.")
        } else {
            None
        };
        match why {
            Some(why) => {
                audit(state, &live, "desk_boss.uplink.refused", why);
                refused.push(json!({"index": i, "why": why}));
            }
            None => {
                if live.token.id == first.token.id {
                    first_proved = true;
                }
                audit(
                    state,
                    &live,
                    "desk_boss.uplink.connect",
                    &hello.engine.version,
                );
                socket.links.lock().insert(
                    live.link.link_id,
                    OnSocket {
                        token: Zeroizing::new(l.token.clone()),
                        group_key: live.link.group_key.clone(),
                        tools: tools.clone(),
                        person: live.person.id,
                        agent_id: live.link.agent_id.clone(),
                        engine_name: live.link.engine_name.clone(),
                        unchecked: 0,
                    },
                );
                accepted.push((live.link.link_id, live));
            }
        }
    }
    if !first_proved {
        socket.links.lock().clear();
        return Err("The link this uplink opened with wasn't proved.".into());
    }
    let links: Vec<Value> = accepted
        .iter()
        .map(|(id, live)| {
            json!({"linkId": id, "tokenId": live.token.id, "group": live.link.group_name,
                   "identityId": live.person.id})
        })
        .collect();
    let ready = json!({"type": "ready", "links": links, "refused": refused,
                       "engine": hello.engine.name, "features": ["asks", "decide"]});
    let _ = socket.tx.try_send(Message::Text(ready.to_string().into()));
    Ok(accepted)
}

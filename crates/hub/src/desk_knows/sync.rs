//! The Mac's side of Sees (`designs/agent-sees.md` §4 and §8 in
//! workforce-desk): this hub's Desk sends each agent what its rule covers
//! of the person's work, and takes back what the person deletes.
//!
//! - **What** is [`Desk::knows_pending`]: one pure `resolve` of the rule,
//!   every string redacted with [`HubRedactor`] here, before anything
//!   leaves. Recorded as sent ([`Desk::knows_mark_sent`]) only once the
//!   agent's hub took it, so nothing is lost or sent twice.
//! - **Where**: an agent of this hub (`local/<agent>`) in process, into
//!   [`super::installed`], as the person whose Desk this is; an agent on
//!   another hub (`<hub>/<agent>`) through its `wfk_` token, which Desk's
//!   native side hands over (`PUT /api/desk/knows-links/{hub}/{agent}`) and
//!   which is kept in this hub's vault at `desk/links/knows/<hub>/<agent>`,
//!   a Desk link no agent tool or secret marker reaches. Never logged.
//! - **Deletes** the person chose, or a session forgotten, go through the
//!   agent's hub's `forget`, retried until it answers.
//! - **Limits**: `[desk.knows]` `send_per_minute` and `send_mb_per_day` per
//!   agent; batches of at most 256 items or 1 MiB; a failed agent backs off
//!   from 5 s, doubling to 10 min.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::put;
use axum::{Json, Router};
use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::settings::{AgentWorkspaceConfig, DeskKnowsSyncConfig, VaultActor};
use workforce_desk::knows_items::Outgoing;
use workforce_desk::knows_store::LogEntry;
use workforce_desk::{Desk, KnowsGather};

use super::store::{Forget, Item, Owner};
use crate::desk_memory::HubRedactor;
use crate::gateway::{resolve_caller_context, AuthFailure, GatewayState};

/// Where an agent's link is kept, under the Desk links no agent reaches.
pub const LINKS: &str = "desk/links/knows";

/// The `source` this hub's own Desk pushes its agents' copies as.
pub const LOCAL_SOURCE: &str = "local";

/// The most items and bytes of one push.
const BATCH_ITEMS: usize = 256;
const BATCH_BYTES: usize = 1024 * 1024;
/// The most refs of one forget.
const FORGET_ITEMS: usize = 2_000;
/// How often every agent's turn comes round.
const EVERY: Duration = Duration::from_secs(30);
const BACKOFF_FIRST: Duration = Duration::from_secs(5);
const BACKOFF_MOST: Duration = Duration::from_secs(600);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// `agent`'s link's vault path, for the member key `<hub>/<agent>`.
pub fn link_key(hub: &str, agent: &str) -> String {
    format!("{LINKS}/{hub}/{agent}")
}

/// What is kept for an agent on another hub. The token is never printed.
#[derive(Clone, Serialize, Deserialize)]
struct Link {
    url: String,
    token: String,
    person: String,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("url", &self.url)
            .field("token", &"wfk_…redacted")
            .finish()
    }
}

fn token_ok(token: &str) -> bool {
    token.starts_with(workforce_identity::remote_mcp_token::DESK_KNOWS_TOKEN_PREFIX)
        && (21..200).contains(&token.len())
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Where an agent's copies go.
enum Target {
    Local(Owner),
    Remote(Link),
}

/// Why a send didn't land.
#[derive(Debug)]
enum Failed {
    /// Try again after the back-off.
    Retry(String),
    /// Refused as it stands (sharing off there, the agent declines, the
    /// token gone): try again only after the longest back-off.
    Refused(String),
    /// What was sent was refused as it is (400, 413): sent again it would
    /// be again.
    Rejected(String),
}

impl Failed {
    fn words(&self) -> &str {
        match self {
            Self::Retry(w) | Self::Refused(w) | Self::Rejected(w) => w,
        }
    }
}

/// What was sent to one agent lately, for its limits.
#[derive(Default)]
struct Spent {
    sent: VecDeque<(Instant, usize, usize)>,
    backoff: Option<(Instant, Duration)>,
    /// The digest the agent's hub last took; none since this hub started.
    digest: Option<&'static str>,
}

impl Spent {
    fn room(&mut self, now: Instant, limits: &DeskKnowsSyncConfig) -> (usize, usize) {
        let day = Duration::from_secs(24 * 60 * 60);
        while self
            .sent
            .front()
            .is_some_and(|(at, _, _)| now.duration_since(*at) > day)
        {
            self.sent.pop_front();
        }
        let minute: usize = self
            .sent
            .iter()
            .filter(|(at, _, _)| now.duration_since(*at) <= Duration::from_secs(60))
            .map(|(_, n, _)| n)
            .sum();
        let bytes: usize = self.sent.iter().map(|(_, _, b)| b).sum();
        let items = (limits.send_per_minute as usize).saturating_sub(minute);
        let bytes = (limits.send_mb_per_day as usize * 1024 * 1024).saturating_sub(bytes);
        (items.min(BATCH_ITEMS), bytes.min(BATCH_BYTES))
    }

    fn waiting(&self, now: Instant) -> bool {
        self.backoff.is_some_and(|(until, _)| now < until)
    }

    fn failed(&mut self, now: Instant, refused: bool) {
        let next = match (refused, self.backoff) {
            (true, _) => BACKOFF_MOST,
            (false, Some((_, last))) => (last * 2).min(BACKOFF_MOST),
            (false, None) => BACKOFF_FIRST,
        };
        self.backoff = Some((now + next, next));
    }
}

/// Who the person whose Desk this is, for this hub's own agents' copies.
pub type PersonOf = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// The sync.
pub struct KnowsSync {
    state: Arc<GatewayState>,
    desk: Desk,
    limits: DeskKnowsSyncConfig,
    person: PersonOf,
    http: reqwest::Client,
    spent: Mutex<HashMap<String, Spent>>,
    /// Each delete's back-off, so one hub out of reach holds up no other.
    retry_delete: Mutex<HashMap<Uuid, Spent>>,
    wake: tokio::sync::Notify,
}

impl KnowsSync {
    pub fn new(
        state: Arc<GatewayState>,
        desk: Desk,
        limits: DeskKnowsSyncConfig,
        person: PersonOf,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(CALL_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            state,
            desk,
            limits,
            person,
            http,
            spent: Mutex::new(HashMap::new()),
            retry_delete: Mutex::new(HashMap::new()),
            wake: tokio::sync::Notify::new(),
        }
    }

    /// A pass at once, not on the next turn.
    pub fn kick(&self) {
        self.wake.notify_one();
    }

    /// The loop, until `cancel`.
    pub fn start(self: &Arc<Self>, cancel: CancellationToken) {
        if !self.limits.enabled {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = me.wake.notified() => {}
                    _ = tokio::time::sleep(EVERY) => {}
                }
                me.pass().await;
            }
        });
    }

    /// One pass: what the person deleted first, then what each rule covers.
    pub async fn pass(&self) {
        self.deletes().await;
        let rules = match self.desk.store().knows_rules().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "agents' rules weren't read");
                return;
            }
        };
        for kept in rules {
            if let Err(e) = self.send(&kept.agent, &kept.rule).await {
                tracing::debug!(agent = %kept.agent, error = %e.words(), "an agent's share is waiting");
            }
        }
    }

    /// Where `agent`'s copies go now, if anywhere. For `taking_back` what
    /// it holds, whether it still takes Desk work doesn't matter: a person
    /// can always take back what was sent.
    fn target(&self, agent: &str, taking_back: bool) -> Option<Target> {
        let (hub, id) = agent.split_once('/')?;
        if hub == workforce_desk::rooms::LOCAL_HUB {
            let person = Uuid::parse_str(&(self.person)()?).ok()?;
            let service = self.state.registry.get(id);
            let accepts = service.as_ref().is_some_and(|service| {
                service.config.desk_knows.enabled
                    && AgentWorkspaceConfig::load(&service.workspace)
                        .ok()
                        .flatten()
                        .is_none_or(|c| c.accepts_desk_knows())
            });
            let agent = service.map_or_else(|| id.to_string(), |s| s.agent_id.to_string());
            return (accepts || taking_back).then(|| {
                Target::Local(Owner {
                    person,
                    source: LOCAL_SOURCE.into(),
                    agent,
                })
            });
        }
        let vault = self.state.vault.as_ref()?;
        let link: Link = serde_json::from_str(&vault.get(&link_key(hub, id))?).ok()?;
        token_ok(&link.token).then_some(Target::Remote(link))
    }

    /// Sends `agent` what `rule` covers that it doesn't hold yet, a batch at
    /// a time, within its limits.
    async fn send(&self, agent: &str, rule: &workforce_desk::knows::Rule) -> Result<(), Failed> {
        let now = Instant::now();
        let (items, bytes) = {
            let mut spent = self.spent.lock();
            let s = spent.entry(agent.to_string()).or_default();
            if s.waiting(now) {
                return Ok(());
            }
            s.room(now, &self.limits)
        };
        if items == 0 || bytes == 0 {
            return Ok(());
        }
        let Some(target) = self.target(agent, false) else {
            return Ok(());
        };
        let home = std::env::var("HOME").ok();
        let pending = self
            .desk
            .knows_pending(
                agent,
                rule,
                &KnowsGather {
                    items,
                    bytes,
                    backfill: chrono::Duration::days(i64::from(self.limits.backfill_days)),
                },
                &HubRedactor,
                home.as_deref(),
                Utc::now(),
            )
            .await
            .map_err(|e| Failed::Retry(e.to_string()))?;
        // A machine's engine leaves how the agent hears of the work to the
        // Mac's: two engines would otherwise say it in turn.
        let digest = (!self.desk.knows_on_machine()).then(|| rule.digest.as_str());
        let told =
            digest.is_none() || self.spent.lock().get(agent).and_then(|s| s.digest) == digest;
        if pending.is_empty() && told {
            return Ok(());
        }
        let mut size: usize = pending.iter().map(Outgoing::bytes).sum();
        let mut landed = self.push(&target, &pending, digest).await;
        let mut count = pending.len();
        let mut done: Vec<Outgoing> = Vec::new();
        // One item the agent's hub won't take must not hold up the rest:
        // each is tried alone, and one refused as it is is passed over (it
        // would be refused again), named in the log by its kind and ref.
        // When none at all is taken the hub is refusing the feed (a version
        // it can't read, say), so nothing is marked and the agent backs off:
        // otherwise its whole feed would be counted as held and never resent.
        let one_by_one = matches!(landed, Err(Failed::Rejected(_)));
        if one_by_one {
            landed = Ok(());
            count = 0;
            size = 0;
            let mut passed = Vec::new();
            for o in &pending {
                match self.push(&target, std::slice::from_ref(o), digest).await {
                    Ok(()) => {
                        count += 1;
                        size += o.bytes();
                        done.push(o.clone());
                    }
                    Err(Failed::Rejected(why)) => {
                        tracing::warn!(agent = %agent, kind = %o.held.kind, reference = %o.held.reference, why = %why, "the agent's hub won't take an item; passed over");
                        passed.push(o.clone());
                    }
                    Err(e) => {
                        landed = Err(e);
                        break;
                    }
                }
            }
            if landed.is_ok() && count == 0 {
                landed = Err(Failed::Refused(
                    "the agent's hub took none of the items".to_string(),
                ));
            } else {
                done.extend(passed);
            }
        }
        {
            let mut spent = self.spent.lock();
            let s = spent.entry(agent.to_string()).or_default();
            match &landed {
                Ok(()) => {
                    s.backoff = None;
                    s.digest = digest;
                    s.sent.push_back((now, count, size));
                }
                Err(e) => {
                    if count > 0 {
                        s.sent.push_back((now, count, size));
                    }
                    s.failed(now, matches!(e, Failed::Refused(_)))
                }
            }
        }
        let marked: &[Outgoing] = if one_by_one { &done } else { &pending };
        if !marked.is_empty() {
            self.desk
                .knows_mark_sent(agent, marked, Utc::now())
                .await
                .map_err(|e| Failed::Retry(e.to_string()))?;
        }
        landed?;
        tracing::info!(target: "sentinel.audit", agent = %agent, items = count, bytes = size, "Desk shared work with an agent");
        Ok(())
    }

    async fn push(
        &self,
        target: &Target,
        pending: &[Outgoing],
        digest: Option<&str>,
    ) -> Result<(), Failed> {
        let items: Vec<Item> = pending
            .iter()
            .map(|o| Item {
                kind: o.held.kind.clone(),
                reference: o.held.reference.clone(),
                session: o.held.session.map(|s| s.to_string()),
                group_name: o.group_name.clone(),
                at: o.at,
                body: o.body.clone(),
            })
            .collect();
        match target {
            Target::Local(owner) => {
                let store = super::installed()
                    .ok_or_else(|| Failed::Refused("this hub keeps no Desk work".into()))?;
                if let Some(digest) = digest {
                    store
                        .set_digest(owner, digest, Utc::now())
                        .await
                        .map_err(|e| Failed::Retry(format!("{e:#}")))?;
                }
                store
                    .push(owner, &items, Utc::now())
                    .await
                    .map(|_| ())
                    .map_err(|e| Failed::Retry(format!("{e:#}")))
            }
            Target::Remote(link) => {
                let mut body = json!({
                    "items": items.iter().map(|i| json!({
                        "kind": i.kind,
                        "ref": i.reference,
                        "session": i.session,
                        "group": i.group_name,
                        "at": i.at,
                        "body": i.body,
                    })).collect::<Vec<_>>(),
                });
                if let Some(digest) = digest {
                    body["digest"] = json!(digest);
                }
                self.call(link, "push", &body).await
            }
        }
    }

    async fn call(&self, link: &Link, route: &str, body: &serde_json::Value) -> Result<(), Failed> {
        let url = format!("{}/api/desk-knows/{route}", link.url.trim_end_matches('/'));
        let reply = self
            .http
            .post(url)
            .bearer_auth(&link.token)
            .json(body)
            .send()
            .await
            .map_err(|e| Failed::Retry(format!("couldn't reach the hub: {}", e.without_url())))?;
        let status = reply.status();
        if status.is_success() {
            return Ok(());
        }
        let words = reply
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("error")?.as_str().map(str::to_string))
            .unwrap_or_else(|| status.to_string());
        if matches!(
            status,
            StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE
        ) {
            Err(Failed::Rejected(words))
        } else if matches!(
            status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ) {
            Err(Failed::Refused(words))
        } else {
            Err(Failed::Retry(words))
        }
    }

    /// Every delete waiting on an agent's hub, tried once each pass.
    async fn deletes(&self) {
        let pending = match self.desk.store().knows_pending_deletes().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "pending deletes weren't read");
                return;
            }
        };
        let now = Instant::now();
        for (agent, log) in pending {
            if self
                .retry_delete
                .lock()
                .get(&log.id)
                .is_some_and(|s| s.waiting(now))
            {
                continue;
            }
            match self.delete(&agent, &log).await {
                Ok(()) => {
                    self.retry_delete.lock().remove(&log.id);
                }
                Err(e) => {
                    self.retry_delete
                        .lock()
                        .entry(log.id)
                        .or_default()
                        .failed(now, false);
                    tracing::debug!(agent = %agent, error = %e.words(), "a delete is waiting on the agent's hub");
                }
            }
        }
    }

    async fn delete(&self, agent: &str, log: &LogEntry) -> Result<(), Failed> {
        let store = self.desk.store();
        let items = store
            .knows_deleting(agent, log.id)
            .await
            .map_err(|e| Failed::Retry(format!("{e:#}")))?;
        if !items.is_empty() {
            // An agent with no way to it now (unlinked, removed) holds
            // nothing Desk can reach: its hub forgot it with the link, or
            // this hub's own store did with the agent. The delete waits for
            // a link, if one comes back.
            let Some(target) = self.target(agent, true) else {
                return Err(Failed::Retry("no way to the agent's hub".into()));
            };
            for chunk in items.chunks(FORGET_ITEMS) {
                match &target {
                    Target::Local(owner) => {
                        let Some(kept) = super::installed() else {
                            return Err(Failed::Retry("this hub keeps no Desk work".into()));
                        };
                        kept.forget(
                            owner,
                            &Forget {
                                items: chunk.to_vec(),
                                ..Forget::default()
                            },
                        )
                        .await
                        .map_err(|e| Failed::Retry(format!("{e:#}")))?;
                    }
                    Target::Remote(link) => {
                        self.call(link, "forget", &json!({ "items": chunk }))
                            .await?;
                    }
                }
            }
        }
        store
            .knows_deleted(agent, log.id, Utc::now())
            .await
            .map_err(|e| Failed::Retry(format!("{e:#}")))?;
        tracing::info!(target: "sentinel.audit", agent = %agent, items = items.len(), "Desk took back what an agent was sent");
        Ok(())
    }
}

// ── The link, handed over by Desk's native side ──────────────────────────

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

#[derive(Deserialize)]
struct LinkBody {
    url: String,
    token: String,
}

fn refusal(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

/// The person whose hub this is, asking as themselves.
async fn holder(
    state: &Arc<GatewayState>,
    headers: &HeaderMap,
    token: Option<&str>,
) -> Result<String, Response> {
    let caller = resolve_caller_context(state, headers, token).await;
    match caller.auth_failure {
        Some(AuthFailure::InvalidCredentials) => {
            return Err(refusal(StatusCode::UNAUTHORIZED, "not allowed"))
        }
        Some(AuthFailure::StoreError) => {
            return Err(refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "the hub could not check that token",
            ))
        }
        None => {}
    }
    let Some(person) = caller.identity_id else {
        return Err(refusal(
            StatusCode::FORBIDDEN,
            "What Desk shares is a person's: ask as yourself, not as the hub",
        ));
    };
    if !crate::desk_floor::holds_the_hub(state.identity_store.as_ref(), true, Some(&person)) {
        return Err(refusal(
            StatusCode::FORBIDDEN,
            "Only the person whose Desk this is links its agents",
        ));
    }
    Ok(person)
}

fn ids_ok(hub: &str, agent: &str) -> bool {
    hub != workforce_desk::rooms::LOCAL_HUB
        && workforce_desk::check_hub_id(hub).is_ok()
        && crate::desk_board_sync::agent_id_ok(agent)
}

/// `PUT /api/desk/knows-links/{hub}/{agent}` `{url, token}`: keeps the
/// agent's `wfk_` token (write-only).
async fn put_link(
    State(r): State<Arc<KnowsSync>>,
    Path((hub, agent)): Path<(String, String)>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
    Json(body): Json<LinkBody>,
) -> Response {
    let person = match holder(&r.state, &headers, q.token.as_deref()).await {
        Ok(p) => p,
        Err(refused) => return refused,
    };
    if !ids_ok(&hub, &agent) {
        return refusal(StatusCode::BAD_REQUEST, "That isn't an agent on a hub.");
    }
    let url = match workforce_desk::check_hub_url(&body.url) {
        Ok(u) => u,
        Err(e) => return e.into_response(),
    };
    let token = body.token.trim();
    if !token_ok(token) {
        return refusal(StatusCode::BAD_REQUEST, "That isn't a Desk work token.");
    }
    let Some(vault) = r.state.vault.as_ref() else {
        return refusal(StatusCode::SERVICE_UNAVAILABLE, "This hub has no vault.");
    };
    let link = Link {
        url: url.as_str().trim_end_matches('/').to_string(),
        token: token.to_string(),
        person: person.clone(),
    };
    let value = serde_json::to_string(&link).unwrap_or_default();
    if let Err(e) = vault.set_with_actor(&link_key(&hub, &agent), &value, VaultActor::User(&person))
    {
        tracing::error!(error = %format!("{e:#}"), hub = %hub, agent = %agent, "an agent's link wasn't kept");
        return refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Desk couldn't keep the link. Try again.",
        );
    }
    tracing::info!(target: "sentinel.audit", hub = %hub, agent = %agent, person = %person, "an agent's link for Desk work was kept");
    // A token refused before is no reason to wait now, and a hub linked
    // anew is told the digest again.
    if let Some(s) = r.spent.lock().get_mut(&format!("{hub}/{agent}")) {
        s.backoff = None;
        s.digest = None;
    }
    r.kick();
    StatusCode::NO_CONTENT.into_response()
}

/// `DELETE /api/desk/knows-links/{hub}/{agent}`: forgets the link; nothing
/// more is sent.
async fn delete_link(
    State(r): State<Arc<KnowsSync>>,
    Path((hub, agent)): Path<(String, String)>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    let person = match holder(&r.state, &headers, q.token.as_deref()).await {
        Ok(p) => p,
        Err(refused) => return refused,
    };
    if !ids_ok(&hub, &agent) {
        return refusal(StatusCode::BAD_REQUEST, "That isn't an agent on a hub.");
    }
    let Some(vault) = r.state.vault.as_ref() else {
        return refusal(StatusCode::SERVICE_UNAVAILABLE, "This hub has no vault.");
    };
    match vault.delete_with_actor(&link_key(&hub, &agent), VaultActor::User(&person)) {
        Ok(_) => {
            tracing::info!(target: "sentinel.audit", hub = %hub, agent = %agent, person = %person, "an agent's link for Desk work was forgotten");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "an agent's link wasn't forgotten");
            refusal(StatusCode::INTERNAL_SERVER_ERROR, "Try again.")
        }
    }
}

/// The link routes.
pub fn router(sync: Arc<KnowsSync>) -> Router {
    Router::new()
        .route(
            "/api/desk/knows-links/{hub}/{agent}",
            put(put_link).delete(delete_link),
        )
        .with_state(sync)
}

/// The sync and its routes, for the desk driver.
pub fn install(
    state: &Arc<GatewayState>,
    desk: Desk,
    limits: DeskKnowsSyncConfig,
    person: PersonOf,
) -> (Arc<KnowsSync>, Router) {
    let sync = Arc::new(KnowsSync::new(Arc::clone(state), desk, limits, person));
    let routes = router(Arc::clone(&sync));
    (sync, routes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_hold_a_minute_and_a_day_and_failures_back_off() {
        let limits = DeskKnowsSyncConfig {
            send_per_minute: 300,
            send_mb_per_day: 1,
            ..DeskKnowsSyncConfig::default()
        };
        let mut s = Spent::default();
        let t0 = Instant::now();
        assert_eq!(s.room(t0, &limits), (BATCH_ITEMS, BATCH_BYTES));
        s.sent.push_back((t0, 200, 512 * 1024));
        assert_eq!(s.room(t0, &limits), (100, 512 * 1024));
        // A minute on, the items are back; the day's bytes aren't.
        let later = t0 + Duration::from_secs(61);
        assert_eq!(s.room(later, &limits), (BATCH_ITEMS, 512 * 1024));
        assert!(!s.waiting(t0));
        s.failed(t0, false);
        assert!(s.waiting(t0));
        assert_eq!(s.backoff.unwrap().1, BACKOFF_FIRST);
        s.failed(t0, false);
        assert_eq!(s.backoff.unwrap().1, BACKOFF_FIRST * 2);
        s.failed(t0, true);
        assert_eq!(s.backoff.unwrap().1, BACKOFF_MOST);
    }

    #[test]
    fn a_link_is_never_printed_and_only_a_desk_work_token_is_kept() {
        let link = Link {
            url: "https://hub.example".into(),
            token: format!("wfk_{}", "a".repeat(40)),
            person: "p".into(),
        };
        assert!(!format!("{link:?}").contains("aaaa"));
        assert!(token_ok(&link.token));
        assert!(!token_ok(&format!("wfr_{}", "a".repeat(40))));
        assert!(!token_ok("wfk_short"));
        assert!(!ids_ok("local", "allan"));
        assert_eq!(
            link_key("driftwerk", "allan"),
            "desk/links/knows/driftwerk/allan"
        );
        assert!(workforce_common::vault_scope::is_desk_link(&link_key(
            "driftwerk",
            "allan"
        )));
    }
}

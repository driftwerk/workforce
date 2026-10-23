//! Signing a person in to ChatGPT from the hub, with Codex's device-code
//! login: the hub asks for a one-time code, the person enters it at
//! `auth.openai.com/codex/device` within fifteen minutes, and the hub polls
//! until the sign-in is granted, exchanges the grant for tokens and stores
//! the `auth.json` Codex would have written in the slot the flow was for.
//!
//! Flows live in the global vault under a prefix only the hub writes, one
//! record per person. Every change to a record is a transition made under
//! that record's lock against the record as stored, so a flow settles once,
//! on whichever replica got there first, and its listeners hear it once.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::settings::{SecretStore, VaultActor};
use workforce_common::subscription::{CredentialTier, SubscriptionKind};

use super::chatgpt_auth::{Account, AuthJson, TokenSet, CLIENT_ID, PASTE_FALLBACK};
use super::resolve::{known_slots, slot_key, VaultSlot};
use super::store::{store_checked, withdraw_login, LoginRefusal};
use crate::hosts::RegistryView;
use crate::leader_gate::LeaderGate;
use crate::vault::Vault;

/// Where ChatGPT sign-ins are issued.
const ISSUER: &str = "https://auth.openai.com";
/// Where the hub keeps each person's flow: a prefix agents can neither
/// write nor read, and members can't reach under `users/`.
pub const RECORD_PREFIX: &str = "subscription/device_login/";
/// Whether `key` is a person's flow record, which keeps their code.
pub fn is_record(key: &str) -> bool {
    key.starts_with(RECORD_PREFIX)
}

/// How long a code may be entered, as Codex allows.
pub const FLOW_LIFETIME: TimeDelta = TimeDelta::minutes(15);
/// The shortest wait between polls of one flow.
pub const MIN_INTERVAL_SECS: u64 = 5;
/// The longest wait a throttled flow backs off to.
const MAX_INTERVAL_SECS: u64 = 60;
/// How long a settled flow's record outlives it.
const KEEP_SETTLED: TimeDelta = TimeDelta::hours(1);
/// How often the poller looks at the flows.
const TICK: Duration = Duration::from_secs(2);
/// What a person is asked to keep in mind before entering a code.
pub const CODE_WARNING: &str =
    "Continue only if you started this sign-in. If a website or another person gave you this code, ignore it.";

/// What a person whose account can't sign in from the hub is told to do
/// instead.
pub fn unsupported_reason() -> String {
    format!("device-code sign-in isn't enabled for this ChatGPT account: {PASTE_FALLBACK}")
}

/// A tier a person signs in to from the hub.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "tier", rename_all = "snake_case")]
pub enum LoginTarget {
    GlobalUser,
    AgentUser { agent_id: String },
    AgentShared { agent_id: String },
}

impl LoginTarget {
    /// The sign-in that fills `tier`, on `agent_id` for an agent's tier:
    /// none for the install's own login, which is pasted, or for an agent's
    /// tier with no agent.
    pub fn new(tier: CredentialTier, agent_id: Option<String>) -> Option<Self> {
        match (tier, agent_id) {
            (CredentialTier::GlobalUser, _) => Some(Self::GlobalUser),
            (CredentialTier::AgentUser, Some(agent_id)) => Some(Self::AgentUser { agent_id }),
            (CredentialTier::AgentShared, Some(agent_id)) => Some(Self::AgentShared { agent_id }),
            _ => None,
        }
    }

    /// The tier the sign-in fills.
    pub const fn tier(&self) -> CredentialTier {
        match self {
            Self::GlobalUser => CredentialTier::GlobalUser,
            Self::AgentUser { .. } => CredentialTier::AgentUser,
            Self::AgentShared { .. } => CredentialTier::AgentShared,
        }
    }

    /// The agent whose vault the login goes in, if not the global one.
    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Self::GlobalUser => None,
            Self::AgentUser { agent_id } | Self::AgentShared { agent_id } => Some(agent_id),
        }
    }
}

/// Where a flow stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FlowState {
    Pending,
    Completed {
        account: Account,
    },
    Failed {
        reason: String,
    },
    Expired,
    Cancelled,
    /// A flow for another target took its place.
    Replaced,
    /// Device login is not enabled for this account: paste a login instead.
    Unsupported,
}

impl FlowState {
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }
}

/// A Slack message that carried a flow's code, so it can be settled when the
/// flow ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackDelivery {
    pub workspace_id: String,
    pub channel: String,
    pub ts: String,
}

/// One device-code sign-in.
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceFlow {
    pub id: Uuid,
    pub identity: Uuid,
    pub target: LoginTarget,
    pub user_code: String,
    pub verification_url: String,
    device_auth_id: String,
    pub interval_secs: u64,
    pub started_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub state: FlowState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at: Option<DateTime<Utc>>,
    /// What wants to hear when the flow ends, as each listener stored it.
    #[serde(default)]
    pub waiters: Vec<Value>,
    #[serde(default)]
    pub deliveries: Vec<SlackDelivery>,
}

impl std::fmt::Debug for DeviceFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceFlow")
            .field("id", &self.id)
            .field("identity", &self.identity)
            .field("target", &self.target)
            .field("state", &self.state)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl DeviceFlow {
    fn settle(self, state: FlowState, now: DateTime<Utc>) -> Self {
        Self {
            state,
            settled_at: Some(now),
            ..self
        }
    }

    /// Whether the flow still waits for its code at `now`, for a target
    /// `joins` accepts.
    fn live_for(&self, joins: impl Fn(&LoginTarget) -> bool, now: DateTime<Utc>) -> bool {
        self.state.is_pending() && self.expires_at > now && joins(&self.target)
    }

    /// The flow with `waiter` among its waiters, and whether that changed it.
    fn joined(mut self, waiter: Option<Value>) -> (Self, bool) {
        match waiter.filter(|waiter| !self.waiters.contains(waiter)) {
            Some(waiter) => {
                self.waiters.push(waiter);
                (self, true)
            }
            None => (self, false),
        }
    }
}

/// Hears every flow that settles, once, on the replica that settled it.
#[async_trait]
pub trait DeviceLoginListener: Send + Sync {
    async fn settled(&self, flow: DeviceFlow);
}

/// Why a flow could not be started or changed.
#[derive(Debug, thiserror::Error)]
pub enum DeviceLoginError {
    #[error("ChatGPT sign-in from the hub needs the hub's global vault")]
    NoVault,
    #[error("could not start a ChatGPT sign-in: {0}")]
    Start(String),
    #[error(transparent)]
    Vault(#[from] eyre::Report),
}

/// Why a call to the auth server got no usable answer.
#[derive(Debug, thiserror::Error)]
enum AuthCallError {
    /// Worth trying again: the call didn't get through, was throttled, or
    /// the server had a moment.
    #[error("the auth server could not be reached: {0}")]
    Transient(String),
    #[error("{0}")]
    Refused(String),
}

/// The response to a call the auth server answered with success; `what`
/// names the call in a failure.
fn answered(
    sent: reqwest::Result<reqwest::Response>,
    what: &str,
) -> Result<reqwest::Response, AuthCallError> {
    let response = sent.map_err(|e| AuthCallError::Transient(e.to_string()))?;
    match response.status() {
        status if status.is_success() => Ok(response),
        status if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() => {
            Err(AuthCallError::Transient(format!(
                "{what} got status {status}"
            )))
        }
        status => Err(AuthCallError::Refused(format!(
            "{what} was refused with status {status}"
        ))),
    }
}

/// Whether `sent` was answered with one of `statuses`.
fn answered_with(
    sent: &reqwest::Result<reqwest::Response>,
    statuses: &[reqwest::StatusCode],
) -> bool {
    sent.as_ref()
        .is_ok_and(|response| statuses.contains(&response.status()))
}

/// A code the auth server issued.
struct UserCode {
    device_auth_id: String,
    user_code: String,
    interval_secs: u64,
}

/// A granted sign-in, still to be exchanged for tokens.
struct Grant {
    authorization_code: String,
    code_verifier: String,
}

/// What a poll found.
enum Poll {
    Pending,
    /// Asked to slow down, or the server had a moment.
    Throttled,
    Granted(Grant),
}

#[derive(Serialize)]
struct UserCodeRequest<'a> {
    client_id: &'a str,
}

#[derive(Deserialize)]
struct UserCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default, deserialize_with = "interval")]
    interval: u64,
}

/// Codex sends the interval as a string; a number is taken too.
fn interval<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    match Value::deserialize(deserializer)? {
        Value::String(text) => text.trim().parse().map_err(serde::de::Error::custom),
        Value::Number(number) => number
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom("interval is not a whole number")),
        _ => Ok(0),
    }
}

#[derive(Serialize)]
struct PollRequest<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

#[derive(Deserialize)]
struct GrantResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

/// Codex's device-code endpoints on an issuer.
#[derive(Clone)]
pub struct DeviceAuthClient {
    http: reqwest::Client,
    issuer: String,
}

impl DeviceAuthClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self::at(http, ISSUER)
    }

    pub fn at(http: reqwest::Client, issuer: &str) -> Self {
        Self {
            http,
            issuer: issuer.trim_end_matches('/').to_string(),
        }
    }

    /// Where a person enters the code.
    pub fn verification_url(&self) -> String {
        format!("{}/codex/device", self.issuer)
    }

    fn accounts(&self, path: &str) -> String {
        format!("{}/api/accounts/deviceauth/{path}", self.issuer)
    }

    /// A new code; `None` when device-code sign-in is not enabled for the
    /// account.
    async fn user_code(&self) -> Result<Option<UserCode>, AuthCallError> {
        let sent = self
            .http
            .post(self.accounts("usercode"))
            .json(&UserCodeRequest {
                client_id: CLIENT_ID,
            })
            .send()
            .await;
        if answered_with(&sent, &[reqwest::StatusCode::NOT_FOUND]) {
            return Ok(None);
        }
        answered(sent, "the code request")?
            .json::<UserCodeResponse>()
            .await
            .map(|code| {
                Some(UserCode {
                    device_auth_id: code.device_auth_id,
                    user_code: code.user_code,
                    interval_secs: code.interval.max(MIN_INTERVAL_SECS),
                })
            })
            .map_err(|e| AuthCallError::Refused(e.to_string()))
    }

    async fn poll(&self, device_auth_id: &str, user_code: &str) -> Result<Poll, AuthCallError> {
        let sent = self
            .http
            .post(self.accounts("token"))
            .json(&PollRequest {
                device_auth_id,
                user_code,
            })
            .send()
            .await;
        if answered_with(
            &sent,
            &[
                reqwest::StatusCode::FORBIDDEN,
                reqwest::StatusCode::NOT_FOUND,
            ],
        ) {
            return Ok(Poll::Pending);
        }
        match answered(sent, "the sign-in") {
            Ok(response) => response
                .json::<GrantResponse>()
                .await
                .map(|grant| {
                    Poll::Granted(Grant {
                        authorization_code: grant.authorization_code,
                        code_verifier: grant.code_verifier,
                    })
                })
                .map_err(|e| AuthCallError::Refused(e.to_string())),
            Err(AuthCallError::Transient(_)) => Ok(Poll::Throttled),
            Err(refused) => Err(refused),
        }
    }

    /// Trade a grant for the sign-in's tokens, as Codex's PKCE exchange does.
    async fn exchange(&self, grant: &Grant) -> Result<TokenSet, AuthCallError> {
        let redirect_uri = format!("{}/deviceauth/callback", self.issuer);
        let sent = self
            .http
            .post(format!("{}/oauth/token", self.issuer))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", grant.authorization_code.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
                ("client_id", CLIENT_ID),
                ("code_verifier", grant.code_verifier.as_str()),
            ])
            .send()
            .await;
        answered(sent, "the token exchange")?
            .json::<TokenResponse>()
            .await
            .map(|tokens| TokenSet {
                id_token: tokens.id_token,
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
            })
            .map_err(|e| AuthCallError::Refused(e.to_string()))
    }
}

/// When a pending flow is next polled, and how long it waits between polls:
/// kept by whichever replica polls.
#[derive(Clone, Copy)]
struct Due {
    at: DateTime<Utc>,
    wait: u64,
}

/// What a poll of a pending flow leaves for the next one.
enum Next {
    Poll,
    BackOff,
    Settled,
}

/// What a transition does to a record.
enum Write {
    Keep,
    Put(Box<DeviceFlow>),
    Remove,
}

struct Inner {
    vault: Option<Arc<Vault>>,
    client: DeviceAuthClient,
    listeners: parking_lot::RwLock<Vec<Arc<dyn DeviceLoginListener>>>,
    due: parking_lot::Mutex<HashMap<Uuid, Due>>,
}

/// The hub's device-code sign-ins.
#[derive(Clone)]
pub struct DeviceLogins {
    inner: Arc<Inner>,
}

impl DeviceLogins {
    /// Whether flow `id` waits on its next poll.
    #[cfg(any(test, feature = "test-support"))]
    pub fn is_due(&self, id: Uuid) -> bool {
        self.inner.due.lock().contains_key(&id)
    }

    /// Whether no flow waits on a poll.
    #[cfg(any(test, feature = "test-support"))]
    pub fn none_due(&self) -> bool {
        self.inner.due.lock().is_empty()
    }

    pub fn new(vault: Option<Arc<Vault>>, client: DeviceAuthClient) -> Self {
        Self {
            inner: Arc::new(Inner {
                vault,
                client,
                listeners: Default::default(),
                due: Default::default(),
            }),
        }
    }

    fn vault(&self) -> Result<&Arc<Vault>, DeviceLoginError> {
        self.inner.vault.as_ref().ok_or(DeviceLoginError::NoVault)
    }

    fn record_key(identity: Uuid) -> String {
        format!("{RECORD_PREFIX}{identity}")
    }

    fn read(vault: &Vault, identity: Uuid) -> Option<DeviceFlow> {
        vault
            .get(&Self::record_key(identity))
            .and_then(|text| serde_json::from_str(&text).ok())
    }

    /// Change `identity`'s record under its lock, against the record as
    /// stored now. Returns what `change` decided, with the flow it settled,
    /// if any, for the listeners.
    async fn transition<T>(
        &self,
        identity: Uuid,
        change: impl FnOnce(Option<DeviceFlow>) -> (Write, T, Option<DeviceFlow>),
    ) -> Result<T, DeviceLoginError> {
        let vault = self.vault()?;
        let key = Self::record_key(identity);
        let (out, settled) = {
            let _guard = vault
                .refresh_lock()
                .lock(&vault.write_location(&key))
                .await?;
            vault.sync()?;
            let (write, out, settled) = change(Self::read(vault, identity));
            match write {
                Write::Keep => {}
                Write::Put(flow) => vault.set_with_actor(
                    &key,
                    &serde_json::to_string(&flow).map_err(eyre::Report::from)?,
                    VaultActor::System("device_login"),
                )?,
                Write::Remove => {
                    vault.delete_with_actor(&key, VaultActor::System("device_login"))?;
                }
            }
            (out, settled)
        };
        if let Some(flow) = settled {
            self.inner.due.lock().remove(&flow.id);
            self.announce(flow).await;
        }
        Ok(out)
    }

    async fn announce(&self, flow: DeviceFlow) {
        let listeners = self.inner.listeners.read().clone();
        for listener in listeners {
            listener.settled(flow.clone()).await;
        }
    }

    /// The pending flow `identity` has for one of `targets`, joined with
    /// `waiter`; `None` when they have none.
    pub async fn join(
        &self,
        identity: Uuid,
        targets: &[LoginTarget],
        waiter: Option<Value>,
    ) -> Result<Option<DeviceFlow>, DeviceLoginError> {
        let now = Utc::now();
        self.transition(identity, |stored| {
            match stored.filter(|flow| flow.live_for(|target| targets.contains(target), now)) {
                Some(flow) => match flow.joined(waiter) {
                    (flow, true) => (Write::Put(Box::new(flow.clone())), Some(flow), None),
                    (flow, false) => (Write::Keep, Some(flow), None),
                },
                None => (Write::Keep, None, None),
            }
        })
        .await
    }

    /// The pending flow `identity` has for `target`, joined with `waiter`;
    /// else a new one, taking over the waiters of a flow they had pending
    /// for another target, which it replaces.
    pub async fn start_or_join(
        &self,
        identity: Uuid,
        target: LoginTarget,
        waiter: Option<Value>,
    ) -> Result<DeviceFlow, DeviceLoginError> {
        if let Some(flow) = self
            .join(identity, std::slice::from_ref(&target), waiter.clone())
            .await?
        {
            return Ok(flow);
        }
        let issued = self.inner.client.user_code().await;
        let verification_url = self.inner.client.verification_url();
        self.transition(identity, move |stored| {
            let now = Utc::now();
            if let Some(flow) = stored
                .clone()
                .filter(|flow| flow.live_for(|live| *live == target, now))
            {
                let (flow, _) = flow.joined(waiter);
                return (Write::Put(Box::new(flow.clone())), Ok(flow), None);
            }
            let (code, state) = match issued {
                Ok(Some(code)) => (code, FlowState::Pending),
                Ok(None) => (
                    UserCode {
                        device_auth_id: String::new(),
                        user_code: String::new(),
                        interval_secs: MIN_INTERVAL_SECS,
                    },
                    FlowState::Unsupported,
                ),
                Err(e) => {
                    return (
                        Write::Keep,
                        Err(DeviceLoginError::Start(e.to_string())),
                        None,
                    )
                }
            };
            let replaced = stored.filter(|flow| flow.state.is_pending()).map(|flow| {
                let state = match flow.expires_at <= now {
                    true => FlowState::Expired,
                    false => FlowState::Replaced,
                };
                flow.settle(state, now)
            });
            let inherited = replaced
                .iter()
                .filter(|flow| flow.state == FlowState::Replaced)
                .flat_map(|flow| flow.waiters.iter().cloned());
            let waiters = inherited
                .chain(waiter)
                .fold(Vec::new(), |mut waiters, waiter| {
                    if !waiters.contains(&waiter) {
                        waiters.push(waiter);
                    }
                    waiters
                });
            let flow = DeviceFlow {
                id: Uuid::new_v4(),
                identity,
                target,
                user_code: code.user_code,
                verification_url,
                device_auth_id: code.device_auth_id,
                interval_secs: code.interval_secs,
                started_at: now,
                expires_at: now + FLOW_LIFETIME,
                settled_at: (!state.is_pending()).then_some(now),
                state,
                waiters,
                deliveries: Vec::new(),
            };
            (Write::Put(Box::new(flow.clone())), Ok(flow), replaced)
        })
        .await?
    }

    /// `identity`'s flow `flow`, if it is theirs and still kept.
    pub fn get(&self, identity: Uuid, flow: Uuid) -> Option<DeviceFlow> {
        let vault = self.inner.vault.as_ref()?;
        vault.sync().ok()?;
        Self::read(vault, identity).filter(|stored| stored.id == flow)
    }

    /// The flow `identity` has for `target`, if any is kept.
    pub fn current(&self, identity: Uuid, target: &LoginTarget) -> Option<DeviceFlow> {
        self.latest(identity)
            .filter(|stored| stored.target == *target)
    }

    /// `identity`'s one kept flow, whatever it signs in to.
    pub fn latest(&self, identity: Uuid) -> Option<DeviceFlow> {
        let vault = self.inner.vault.as_ref()?;
        vault.sync().ok()?;
        Self::read(vault, identity)
    }

    /// End `identity`'s pending flow `flow`. Returns whether it was pending.
    pub async fn cancel(&self, identity: Uuid, flow: Uuid) -> Result<bool, DeviceLoginError> {
        self.settle_pending(identity, flow, FlowState::Cancelled, Utc::now())
            .await
            .map(|settled| settled.is_some())
    }

    /// Keep that `delivery` carried the code of `identity`'s flow `flow`.
    pub async fn note_delivery(
        &self,
        identity: Uuid,
        flow: Uuid,
        delivery: SlackDelivery,
    ) -> Result<(), DeviceLoginError> {
        self.transition(identity, |stored| match stored {
            Some(mut stored) if stored.id == flow => {
                stored.deliveries.push(delivery);
                (Write::Put(Box::new(stored)), (), None)
            }
            _ => (Write::Keep, (), None),
        })
        .await
    }

    pub fn on_complete(&self, listener: Arc<dyn DeviceLoginListener>) {
        self.inner.listeners.write().push(listener);
    }

    /// Poll every pending flow while this hub leads, every [`TICK`], until
    /// `stop` or the registry is gone.
    pub fn spawn_poller(
        &self,
        registry: Weak<dyn RegistryView>,
        leader: Arc<LeaderGate>,
        stop: CancellationToken,
    ) {
        if self.inner.vault.is_none() {
            return;
        }
        let logins = self.clone();
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(TICK);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = ticks.tick() => {}
                }
                if !leader.is_leader() {
                    continue;
                }
                let Some(registry) = registry.upgrade() else {
                    return;
                };
                logins.tick(registry.as_ref(), Utc::now()).await;
            }
        });
    }

    /// Move every flow on as of `now`: expire the overdue, poll the due,
    /// store the granted and prune the long settled.
    pub async fn tick(&self, registry: &dyn RegistryView, now: DateTime<Utc>) {
        let Some(vault) = self.inner.vault.as_ref() else {
            return;
        };
        if let Err(e) = vault.sync() {
            tracing::warn!(error = %e, "device logins not polled: the vault could not be read");
            return;
        }
        let flows: Vec<DeviceFlow> = vault
            .list(Some(RECORD_PREFIX))
            .iter()
            .filter_map(|key| key.strip_prefix(RECORD_PREFIX)?.parse().ok())
            .filter_map(|identity| Self::read(vault, identity))
            .collect();
        self.inner.due.lock().retain(|id, _| {
            flows
                .iter()
                .any(|flow| flow.id == *id && flow.state.is_pending())
        });
        for flow in flows {
            if let Err(e) = self.advance(registry, flow, now).await {
                tracing::warn!(error = %e, "device login not advanced");
            }
        }
    }

    async fn advance(
        &self,
        registry: &dyn RegistryView,
        flow: DeviceFlow,
        now: DateTime<Utc>,
    ) -> Result<(), DeviceLoginError> {
        let (id, identity) = (flow.id, flow.identity);
        match &flow.state {
            FlowState::Pending if now >= flow.expires_at => self
                .settle_pending(identity, id, FlowState::Expired, now)
                .await
                .map(drop),
            FlowState::Pending => {
                let due = *self.inner.due.lock().entry(id).or_insert(Due {
                    at: now,
                    wait: flow.interval_secs.max(MIN_INTERVAL_SECS),
                });
                if now < due.at {
                    return Ok(());
                }
                let next = match self
                    .inner
                    .client
                    .poll(&flow.device_auth_id, &flow.user_code)
                    .await
                {
                    Ok(Poll::Pending) => Ok(Next::Poll),
                    Ok(Poll::Throttled) => Ok(Next::BackOff),
                    Ok(Poll::Granted(grant)) => self.complete(registry, &flow, grant).await,
                    Err(refused) => {
                        let reason = refused.to_string();
                        self.settle_pending(identity, id, FlowState::Failed { reason }, now)
                            .await
                            .map(|_| Next::Settled)
                    }
                };
                let wait = match &next {
                    Ok(Next::Settled) => None,
                    Ok(Next::BackOff) => Some((due.wait * 2).min(MAX_INTERVAL_SECS)),
                    Ok(Next::Poll) | Err(_) => Some(due.wait),
                };
                if let Some(wait) = wait {
                    self.inner.due.lock().insert(
                        id,
                        Due {
                            at: now + TimeDelta::seconds(wait as i64),
                            wait,
                        },
                    );
                }
                next.map(drop)
            }
            _ if flow
                .settled_at
                .unwrap_or(flow.expires_at)
                .checked_add_signed(KEEP_SETTLED)
                .is_some_and(|keep_until| now >= keep_until) =>
            {
                self.transition(identity, |stored| match stored {
                    Some(stored) if stored.id == id && !stored.state.is_pending() => {
                        (Write::Remove, (), None)
                    }
                    _ => (Write::Keep, (), None),
                })
                .await
            }
            _ => Ok(()),
        }
    }

    /// Settle `identity`'s flow `id` in `state`, if it is still pending.
    /// Returns the settled flow.
    async fn settle_pending(
        &self,
        identity: Uuid,
        id: Uuid,
        state: FlowState,
        now: DateTime<Utc>,
    ) -> Result<Option<DeviceFlow>, DeviceLoginError> {
        self.transition(identity, |stored| match stored {
            Some(stored) if stored.id == id && stored.state.is_pending() => {
                let settled = stored.settle(state, now);
                (
                    Write::Put(Box::new(settled.clone())),
                    Some(settled.clone()),
                    Some(settled),
                )
            }
            _ => (Write::Keep, None, None),
        })
        .await
    }

    /// Whether `flow` is still pending as stored.
    async fn still_pending(&self, flow: &DeviceFlow) -> Result<bool, DeviceLoginError> {
        self.transition(flow.identity, |stored| {
            let pending =
                stored.is_some_and(|stored| stored.id == flow.id && stored.state.is_pending());
            (Write::Keep, pending, None)
        })
        .await
    }

    /// Exchange a granted flow's sign-in, store it in the flow's slot and
    /// settle the flow. A sign-in for a flow that ended before it was stored
    /// is dropped, and one stored as the flow ended is withdrawn, unless a
    /// refresh has already replaced it. An exchange that didn't get through
    /// is tried again at the next poll.
    async fn complete(
        &self,
        registry: &dyn RegistryView,
        flow: &DeviceFlow,
        grant: Grant,
    ) -> Result<Next, DeviceLoginError> {
        let signed_in = match self.inner.client.exchange(&grant).await {
            Err(AuthCallError::Transient(reason)) => {
                tracing::info!(flow = %flow.id, %reason, "the sign-in's token exchange will be tried again");
                return Ok(Next::BackOff);
            }
            exchanged => exchanged.map(|tokens| AuthJson::signed_in(tokens, Utc::now())),
        };
        if !self.still_pending(flow).await? {
            tracing::info!(flow = %flow.id, "a granted sign-in arrived for a flow that had ended; dropped unstored");
            return Ok(Next::Settled);
        }
        let (state, stored) = match signed_in {
            Err(refused) => (
                FlowState::Failed {
                    reason: refused.to_string(),
                },
                None,
            ),
            Ok(auth) => self.store(registry, flow, &auth).await?,
        };
        if stored.is_some() {
            reload_coding_sessions(registry, flow.target.agent_id());
        }
        let settled = self
            .settle_pending(flow.identity, flow.id, state, Utc::now())
            .await?;
        if let (None, Some((slot, value))) = (settled, stored) {
            let actor = flow.identity.to_string();
            if withdraw_login(&slot, &value, VaultActor::User(&actor)).await? {
                tracing::info!(flow = %flow.id, "the flow ended while its sign-in was stored; withdrawn");
                reload_coding_sessions(registry, flow.target.agent_id());
            }
        }
        Ok(Next::Settled)
    }

    /// Store `auth` in the slot `flow` is for: the state the flow settles
    /// in, with the slot and the login when it was stored.
    async fn store(
        &self,
        registry: &dyn RegistryView,
        flow: &DeviceFlow,
        auth: &AuthJson,
    ) -> Result<(FlowState, Option<(VaultSlot, String)>), DeviceLoginError> {
        let global = self.vault()?;
        let slot = match target_slot(registry, global, &flow.target, flow.identity) {
            Ok(slot) => slot,
            Err(reason) => return Ok((FlowState::Failed { reason }, None)),
        };
        let known = known_slots(
            SubscriptionKind::ChatGpt,
            &registry.login_vaults(Some(global)),
            Some(&flow.identity.to_string()),
        );
        let value = auth.render();
        let actor = flow.identity.to_string();
        Ok(
            match store_checked(
                &slot,
                SubscriptionKind::ChatGpt,
                &value,
                &known,
                VaultActor::User(&actor),
            )
            .await
            {
                Ok(()) => (
                    FlowState::Completed {
                        account: auth.account(),
                    },
                    Some((slot, value)),
                ),
                Err(LoginRefusal::Vault(e)) => (
                    FlowState::Failed {
                        reason: format!("the sign-in could not be stored: {e}"),
                    },
                    None,
                ),
                Err(refused) => (
                    FlowState::Failed {
                        reason: refused.to_string(),
                    },
                    None,
                ),
            },
        )
    }
}

/// The slot a flow's sign-in is stored in, or why there is none.
pub fn target_slot(
    registry: &dyn RegistryView,
    global: &Arc<Vault>,
    target: &LoginTarget,
    identity: Uuid,
) -> Result<VaultSlot, String> {
    let vault = match target.agent_id() {
        None => Arc::clone(global),
        Some(agent_id) => registry
            .get(agent_id)
            .and_then(|service| service.vault.clone())
            .ok_or_else(|| format!("agent {agent_id} has no vault on this hub any more"))?,
    };
    let key = slot_key(target.tier(), SubscriptionKind::ChatGpt, Some(identity))
        .map_err(|miss| miss.to_string())?;
    Ok(VaultSlot { vault, key })
}

/// Let the agents a login stored or removed on `agent_id` (every agent, for
/// a global login) offer what it unlocks, or withdraw what it no longer
/// does, without a restart.
pub fn reload_coding_sessions(registry: &dyn RegistryView, agent_id: Option<&str>) {
    match agent_id {
        Some(agent_id) => registry.get(agent_id).into_iter().collect::<Vec<_>>(),
        None => registry.all_services(),
    }
    .iter()
    .for_each(|service| service.reload_coding_session());
}

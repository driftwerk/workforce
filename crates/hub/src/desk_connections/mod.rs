//! Settings → Connections: the services a person connects for their local
//! agents, on a hub with `[desk]` only.
//!
//! - `GET /api/desk/connections`: every connector, where it is connected
//!   (for all the person's agents, or just one), what its last check found,
//!   and which agents use it. Never a value.
//! - `POST /api/desk/connections/{connector}` `{level, agent?, fields}`:
//!   tests the keys, then keeps them at the level's vault tier. A failed
//!   Test keeps nothing.
//! - `POST …/{connector}/test` `{level, agent?}`: checks the kept keys again.
//! - `DELETE …/{connector}?level=&agent=`: removes the keys at that tier
//!   alone.
//! - `POST …/{connector}/oauth/start` `{level, agent?}`: GitHub's device
//!   sign-in, or Google's on the person's own OAuth client;
//!   `POST /api/desk/connections/google/client` keeps that client.
//! - `GET|DELETE /api/desk/connection-flows/{flow}`: a sign-in's progress,
//!   or cancelling it.
//!
//! "All my agents" is the vault's `GlobalPerUser` tier and "just this agent"
//! its `AgentPerUser` one. Slack's bot token is shared by its nature, so it
//! is kept at `Global` / `AgentSpecific`. A key a person saved before
//! Connections, at the bare tier, still counts. Requests are the person's
//! own, through their paired device, and only the person whose hub this is
//! may make them.

pub mod catalog;
pub mod check;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;
use uuid::Uuid;
use workforce_common::settings::{VaultActor, VaultSource};
use workforce_desk::connections::{Checked, Level, Status};

use crate::gateway::{resolve_caller_context, AuthFailure, GatewayState};
use crate::vault::{Vault, VaultOp};
use catalog::{Connector, Field, SignIn, CATALOG};
use check::{Found, Services};

/// The largest value one field takes.
const MAX_VALUE: usize = 16 * 1024;
/// How long a sign-in is waited on.
const FLOW_TTL: Duration = Duration::from_secs(15 * 60);

/// A local agent, as Connections sees it.
#[derive(Clone)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub vault: Option<Arc<Vault>>,
}

/// The hub's agents.
pub trait Agents: Send + Sync {
    fn all(&self) -> Vec<Agent>;
    fn get(&self, id: &str) -> Option<Agent> {
        self.all().into_iter().find(|a| a.id == id)
    }
}

/// Where what a check found is kept.
#[async_trait]
pub trait Statuses: Send + Sync {
    async fn all(&self, person: &str) -> eyre::Result<Vec<Checked>>;
    async fn put(&self, checked: &Checked, connected: bool) -> eyre::Result<()>;
    async fn forget(&self, connector: &str, level: &Level, person: &str) -> eyre::Result<()>;
}

#[async_trait]
impl Statuses for workforce_desk::Desk {
    async fn all(&self, person: &str) -> eyre::Result<Vec<Checked>> {
        self.store().connections(person).await
    }
    async fn put(&self, checked: &Checked, connected: bool) -> eyre::Result<()> {
        self.store().put_connection(checked, connected).await
    }
    async fn forget(&self, connector: &str, level: &Level, person: &str) -> eyre::Result<()> {
        self.store().drop_connection(connector, level, person).await
    }
}

/// Why a request was refused.
#[derive(Debug)]
pub enum Refusal {
    Invalid(String),
    NotFound(String),
    Unavailable(String),
    /// The service refused or failed the Test: nothing was kept.
    TestFailed {
        error: String,
        status: Status,
    },
    Internal(eyre::Report),
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            Self::Invalid(e) => (StatusCode::BAD_REQUEST, json!({ "error": e })),
            Self::NotFound(e) => (StatusCode::NOT_FOUND, json!({ "error": e })),
            Self::Unavailable(e) => (StatusCode::SERVICE_UNAVAILABLE, json!({ "error": e })),
            Self::TestFailed { error, status } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({ "error": error, "status": status.as_str() }),
            ),
            Self::Internal(e) => {
                tracing::error!(error = %format!("{e:#}"), "a connections request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "error": "Desk couldn't do that. Try again." }),
                )
            }
        };
        (status, Json(body)).into_response()
    }
}

/// A connection as the page shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionView {
    /// `all` or `agent`.
    pub level: &'static str,
    pub agent: Option<String>,
    pub agent_name: Option<String>,
    pub account: Option<String>,
    /// `connected`, `needs_reconnect` or `failing`.
    pub status: Status,
    pub detail: Option<String>,
    pub connected_at: Option<DateTime<Utc>>,
    pub checked_at: Option<DateTime<Utc>>,
    /// The vault tier it is kept at.
    pub source: VaultSource,
}

/// An agent a connector reaches, and through whose keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsedBy {
    pub agent: String,
    pub name: String,
    /// `mine` (the person's, for all their agents) or `own` (just this
    /// agent's).
    pub connection: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorView {
    pub id: &'static str,
    pub name: &'static str,
    pub about: &'static str,
    pub sign_in: SignIn,
    pub fields: &'static [Field],
    /// Whether its sign-in can start: GitHub's app is set up, or the
    /// person's Google client is kept.
    pub sign_in_ready: bool,
    pub connections: Vec<ConnectionView>,
    pub used_by: Vec<UsedBy>,
}

/// A sign-in started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Started {
    pub flow: String,
    /// Google: the page to open.
    pub url: Option<String>,
    /// GitHub: the code to type at `verification_uri`.
    pub user_code: Option<String>,
    pub verification_uri: Option<String>,
    pub expires_in: u64,
}

/// Where a sign-in is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum FlowState {
    Waiting,
    Connected { account: String },
    Cancelled,
    Expired,
    Failed { error: String },
}

struct Flow {
    person: String,
    level: Level,
    state: FlowState,
    until: Instant,
}

/// Connections, over the hub's vaults and Desk's store.
pub struct Connections {
    pub global: Option<Arc<Vault>>,
    pub agents: Arc<dyn Agents>,
    pub statuses: Arc<dyn Statuses>,
    pub services: Services,
    /// Driftwerk's GitHub OAuth app, for the device sign-in.
    pub github_client_id: Option<String>,
    /// The hub's own Google callback, for a client that names none.
    pub google_redirect: String,
    flows: Mutex<HashMap<String, Flow>>,
}

fn per_user(tier: VaultSource) -> bool {
    matches!(tier, VaultSource::AgentPerUser | VaultSource::GlobalPerUser)
}

/// The key `path` is kept under at `tier` for `person`.
pub fn key_at(tier: VaultSource, person: &str, path: &str) -> String {
    match per_user(tier) {
        true => format!("users/{person}/{path}"),
        false => path.to_string(),
    }
}

/// `text` with every value in `fields` and anything shaped like a secret
/// taken out.
fn scrub(text: &str, fields: &BTreeMap<String, String>) -> String {
    let text = fields
        .values()
        .filter(|v| v.len() >= 4)
        .fold(text.to_string(), |t, v| t.replace(v.as_str(), "[secret]"));
    workforce_common::redact::redact_secrets(&text)
}

impl Connections {
    pub fn new(
        global: Option<Arc<Vault>>,
        agents: Arc<dyn Agents>,
        statuses: Arc<dyn Statuses>,
        services: Services,
        github_client_id: Option<String>,
        google_redirect: String,
    ) -> Self {
        Self {
            global,
            agents,
            statuses,
            services,
            github_client_id: github_client_id.filter(|id| !id.trim().is_empty()),
            google_redirect,
            flows: Mutex::new(HashMap::new()),
        }
    }

    fn connector(id: &str) -> Result<&'static Connector, Refusal> {
        catalog::find(id).ok_or_else(|| Refusal::NotFound(format!("Desk has no connector {id}")))
    }

    fn level(&self, level: &str, agent: Option<&str>) -> Result<Level, Refusal> {
        match (level, agent.map(str::trim).filter(|a| !a.is_empty())) {
            ("all", None) => Ok(Level::All),
            ("agent", Some(agent)) => match self.agents.get(agent) {
                Some(_) => Ok(Level::Agent(agent.to_string())),
                None => Err(Refusal::NotFound(format!("There's no agent {agent}"))),
            },
            ("agent", None) => Err(Refusal::Invalid("Say which agent it's for".into())),
            ("all", Some(_)) => Err(Refusal::Invalid(
                "A connection for all your agents names no agent".into(),
            )),
            _ => Err(Refusal::Invalid("The level is all or agent".into())),
        }
    }

    fn vault_at(&self, tier: VaultSource, level: &Level) -> Result<Arc<Vault>, Refusal> {
        match (tier, level) {
            (VaultSource::GlobalPerUser | VaultSource::Global, _) => {
                self.global.clone().ok_or_else(|| {
                    Refusal::Unavailable("This hub keeps no vault for your agents".into())
                })
            }
            (VaultSource::AgentPerUser | VaultSource::AgentSpecific, Level::Agent(agent)) => self
                .agents
                .get(agent)
                .and_then(|a| a.vault)
                .ok_or_else(|| Refusal::Unavailable(format!("{agent} keeps no vault"))),
            _ => Err(Refusal::Invalid("That tier isn't one of this level".into())),
        }
    }

    /// The tier a connection at `level` is kept at now, if it is connected.
    fn held_at(&self, c: &Connector, level: &Level, person: &str) -> Option<VaultSource> {
        let tiers = c.tiers(matches!(level, Level::Agent(_)));
        tiers.into_iter().find(|tier| {
            self.vault_at(*tier, level)
                .ok()
                .is_some_and(|v| v.get(&key_at(*tier, person, c.primary)).is_some())
        })
    }

    /// The kept fields of a connection at `tier`.
    fn kept(
        &self,
        c: &Connector,
        level: &Level,
        tier: VaultSource,
        person: &str,
    ) -> BTreeMap<String, String> {
        let Ok(vault) = self.vault_at(tier, level) else {
            return BTreeMap::new();
        };
        c.fields
            .iter()
            .filter_map(|f| {
                vault
                    .get(&key_at(tier, person, f.path))
                    .map(|v| (f.name.to_string(), v))
            })
            .collect()
    }

    fn view(&self, level: &Level, tier: VaultSource, checked: Option<&Checked>) -> ConnectionView {
        let agent = match level {
            Level::All => None,
            Level::Agent(agent) => Some(agent.clone()),
        };
        ConnectionView {
            level: level.as_str(),
            agent_name: agent
                .as_deref()
                .and_then(|a| self.agents.get(a))
                .map(|a| a.name),
            agent,
            account: checked.and_then(|c| c.account.clone()),
            status: checked.map_or(Status::Connected, |c| c.status),
            detail: checked.and_then(|c| c.detail.clone()),
            connected_at: checked.map(|c| c.connected_at),
            checked_at: checked.and_then(|c| c.checked_at),
            source: tier,
        }
    }

    fn sign_in_ready(&self, c: &Connector) -> bool {
        match c.sign_in {
            SignIn::Fields => true,
            SignIn::Github => self.github_client_id.is_some(),
            SignIn::Google => self.google_client().is_some(),
        }
    }

    /// The person's Google OAuth client, as the callback reads it.
    fn google_client(&self) -> Option<(String, String, Option<String>)> {
        let global = self.global.as_ref()?;
        Some((
            global.get(catalog::GOOGLE_CLIENT_ID)?,
            global.get(catalog::GOOGLE_CLIENT_SECRET)?,
            global.get(catalog::GOOGLE_REDIRECT_URI),
        ))
    }

    /// Every connector, as `person` has connected it.
    pub async fn list(&self, person: &str) -> Vec<ConnectorView> {
        let checks = self.statuses.all(person).await.unwrap_or_else(|e| {
            warn!(error = %format!("{e:#}"), "connection checks unreadable; showing keys only");
            Vec::new()
        });
        let agents = self.agents.all();
        CATALOG
            .iter()
            .map(|c| {
                let checked = |level: &Level| {
                    checks
                        .iter()
                        .find(|k| k.connector == c.id && &k.level == level)
                };
                let mine = self.held_at(c, &Level::All, person);
                let mut connections: Vec<ConnectionView> = mine
                    .map(|tier| self.view(&Level::All, tier, checked(&Level::All)))
                    .into_iter()
                    .collect();
                let mut used_by = Vec::new();
                for a in &agents {
                    let level = Level::Agent(a.id.clone());
                    let own = self.held_at(c, &level, person);
                    if let Some(tier) = own {
                        connections.push(self.view(&level, tier, checked(&level)));
                    }
                    let connection = match (own, mine) {
                        (Some(_), _) => "own",
                        (None, Some(_)) => "mine",
                        (None, None) => continue,
                    };
                    used_by.push(UsedBy {
                        agent: a.id.clone(),
                        name: a.name.clone(),
                        connection,
                    });
                }
                ConnectorView {
                    id: c.id,
                    name: c.name,
                    about: c.about,
                    sign_in: c.sign_in,
                    fields: c.fields,
                    sign_in_ready: self.sign_in_ready(c),
                    connections,
                    used_by,
                }
            })
            .collect()
    }

    /// The fields as given, checked against the connector's.
    fn fields(
        c: &Connector,
        given: BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>, Refusal> {
        if let Some(name) = given
            .keys()
            .find(|k| !c.fields.iter().any(|f| f.name == *k))
        {
            return Err(Refusal::Invalid(format!("{} has no field {name}", c.name)));
        }
        let mut out = BTreeMap::new();
        for f in c.fields {
            let value = given.get(f.name).map(|v| v.trim()).unwrap_or_default();
            if value.len() > MAX_VALUE {
                return Err(Refusal::Invalid(format!("{} is too long", f.label)));
            }
            match (value.is_empty(), f.required) {
                (true, true) => return Err(Refusal::Invalid(format!("{} is needed", f.label))),
                (true, false) => {}
                (false, _) => {
                    out.insert(f.name.to_string(), value.to_string());
                }
            }
        }
        Ok(out)
    }

    /// Keeps `fields` at the level's tier: every one given, and none of the
    /// optional ones left out.
    fn keep(
        &self,
        c: &Connector,
        level: &Level,
        person: &str,
        fields: &BTreeMap<String, String>,
    ) -> Result<VaultSource, Refusal> {
        let tier = c.tiers(matches!(level, Level::Agent(_)))[0];
        let vault = self.vault_at(tier, level)?;
        let keys: Vec<(String, Option<&String>)> = c
            .fields
            .iter()
            .map(|f| (key_at(tier, person, f.path), fields.get(f.name)))
            .collect();
        let ops: Vec<VaultOp<'_>> = keys
            .iter()
            .map(|(path, value)| match value {
                Some(value) => VaultOp::Set { path, value },
                None => VaultOp::Delete { path },
            })
            .collect();
        vault
            .apply(&ops, VaultActor::User(person))
            .map_err(Refusal::Internal)?;
        Ok(tier)
    }

    async fn record(
        &self,
        c: &Connector,
        level: &Level,
        person: &str,
        found: &Found,
        connected: bool,
    ) {
        let (status, account, detail) = match found {
            Found::Works { account } => (Status::Connected, Some(account.clone()), None),
            Found::Refused(e) => (Status::NeedsReconnect, None, Some(e.clone())),
            Found::Failed(e) => (Status::Failing, None, Some(e.clone())),
        };
        let now = Utc::now();
        let checked = Checked {
            connector: c.id.to_string(),
            level: level.clone(),
            person: person.to_string(),
            account,
            status,
            detail,
            connected_at: now,
            checked_at: Some(now),
        };
        if let Err(e) = self.statuses.put(&checked, connected).await {
            warn!(connector = c.id, error = %format!("{e:#}"), "a connection's check wasn't kept");
        }
    }

    async fn shown(
        &self,
        c: &Connector,
        level: &Level,
        person: &str,
    ) -> Result<ConnectionView, Refusal> {
        let tier = self
            .held_at(c, level, person)
            .ok_or_else(|| Refusal::NotFound(format!("{} isn't connected there", c.name)))?;
        let checks = self.statuses.all(person).await.unwrap_or_default();
        let checked = checks
            .iter()
            .find(|k| k.connector == c.id && &k.level == level);
        Ok(self.view(level, tier, checked))
    }

    /// Tests `fields`, then keeps them. A failed Test keeps nothing.
    pub async fn connect(
        &self,
        person: &str,
        id: &str,
        level: &str,
        agent: Option<&str>,
        given: BTreeMap<String, String>,
    ) -> Result<ConnectionView, Refusal> {
        let c = Self::connector(id)?;
        if c.fields.is_empty() {
            return Err(Refusal::Invalid(format!(
                "{} is connected by signing in",
                c.name
            )));
        }
        let level = self.level(level, agent)?;
        let fields = Self::fields(c, given)?;
        // The tier must be reachable before anything is sent anywhere.
        self.vault_at(c.tiers(matches!(level, Level::Agent(_)))[0], &level)?;
        match self.services.check(c.id, &fields).await {
            Found::Works { account } => {
                self.keep(c, &level, person, &fields)?;
                self.record(c, &level, person, &Found::Works { account }, true)
                    .await;
                self.shown(c, &level, person).await
            }
            Found::Refused(e) => Err(Refusal::TestFailed {
                error: scrub(&e, &fields),
                status: Status::NeedsReconnect,
            }),
            Found::Failed(e) => Err(Refusal::TestFailed {
                error: scrub(&e, &fields),
                status: Status::Failing,
            }),
        }
    }

    /// Checks a kept connection again, and keeps what it found.
    pub async fn test(
        &self,
        person: &str,
        id: &str,
        level: &str,
        agent: Option<&str>,
    ) -> Result<ConnectionView, Refusal> {
        let c = Self::connector(id)?;
        let level = self.level(level, agent)?;
        let tier = self
            .held_at(c, &level, person)
            .ok_or_else(|| Refusal::NotFound(format!("{} isn't connected there", c.name)))?;
        let found = match c.sign_in {
            SignIn::Google => self.test_google(&level, tier, person).await,
            _ => {
                let fields = self.kept(c, &level, tier, person);
                match self.services.check(c.id, &fields).await {
                    Found::Works { account } => Found::Works { account },
                    Found::Refused(e) => Found::Refused(scrub(&e, &fields)),
                    Found::Failed(e) => Found::Failed(scrub(&e, &fields)),
                }
            }
        };
        self.record(c, &level, person, &found, false).await;
        self.shown(c, &level, person).await
    }

    async fn test_google(&self, level: &Level, tier: VaultSource, person: &str) -> Found {
        let Some((id, secret, _)) = self.google_client() else {
            return Found::Refused("Your Google OAuth client isn't saved in Desk any more".into());
        };
        let Ok(vault) = self.vault_at(tier, level) else {
            return Found::Failed("Desk couldn't read this connection".into());
        };
        let Some(refresh) = vault.get(&key_at(
            tier,
            person,
            catalog::find("google").map_or("", |c| c.primary),
        )) else {
            return Found::Refused("Google isn't signed in".into());
        };
        let email = vault.get(&key_at(tier, person, catalog::GOOGLE_EMAIL));
        let mut fields = BTreeMap::new();
        fields.insert("refresh".into(), refresh.clone());
        fields.insert("secret".into(), secret.clone());
        match self.services.google(&id, &secret, &refresh, email).await {
            Found::Refused(e) => Found::Refused(scrub(&e, &fields)),
            Found::Failed(e) => Found::Failed(scrub(&e, &fields)),
            works => works,
        }
    }

    /// Removes a connection's keys at the tier it is kept at, and nothing
    /// else.
    pub async fn disconnect(
        &self,
        person: &str,
        id: &str,
        level: &str,
        agent: Option<&str>,
    ) -> Result<(), Refusal> {
        let c = Self::connector(id)?;
        let level = self.level(level, agent)?;
        let tier = self
            .held_at(c, &level, person)
            .ok_or_else(|| Refusal::NotFound(format!("{} isn't connected there", c.name)))?;
        let vault = self.vault_at(tier, &level)?;
        let keys: Vec<String> = c.held.iter().map(|p| key_at(tier, person, p)).collect();
        let ops: Vec<VaultOp<'_>> = keys.iter().map(|path| VaultOp::Delete { path }).collect();
        vault
            .apply(&ops, VaultActor::User(person))
            .map_err(Refusal::Internal)?;
        if let Err(e) = self.statuses.forget(c.id, &level, person).await {
            warn!(connector = c.id, error = %format!("{e:#}"), "a connection's checks weren't forgotten");
        }
        Ok(())
    }

    /// Keeps the person's own Google OAuth client, hub-wide, where the
    /// callback reads it.
    pub fn keep_google_client(
        &self,
        person: &str,
        client_id: &str,
        client_secret: &str,
    ) -> Result<(), Refusal> {
        let (client_id, client_secret) = (client_id.trim(), client_secret.trim());
        if client_id.is_empty() || client_secret.is_empty() {
            return Err(Refusal::Invalid(
                "The client ID and secret are both needed".into(),
            ));
        }
        if client_id.len() > MAX_VALUE || client_secret.len() > MAX_VALUE {
            return Err(Refusal::Invalid("That client is too long".into()));
        }
        let vault = self.vault_at(VaultSource::Global, &Level::All)?;
        vault
            .apply(
                &[
                    VaultOp::Set {
                        path: catalog::GOOGLE_CLIENT_ID,
                        value: client_id,
                    },
                    VaultOp::Set {
                        path: catalog::GOOGLE_CLIENT_SECRET,
                        value: client_secret,
                    },
                ],
                VaultActor::User(person),
            )
            .map_err(Refusal::Internal)?;
        Ok(())
    }

    fn insert_flow(&self, person: &str, level: Level) -> String {
        let id = Uuid::new_v4().simple().to_string();
        let now = Instant::now();
        let mut flows = self.flows.lock().expect("connection flows poisoned");
        flows.retain(|_, f| f.until > now);
        flows.insert(
            id.clone(),
            Flow {
                person: person.to_string(),
                level,
                state: FlowState::Waiting,
                until: now + FLOW_TTL,
            },
        );
        id
    }

    fn settle(&self, flow: &str, state: FlowState) {
        if let Some(f) = self
            .flows
            .lock()
            .expect("connection flows poisoned")
            .get_mut(flow)
        {
            if f.state == FlowState::Waiting {
                f.state = state;
            }
        }
    }

    fn waiting(&self, flow: &str) -> bool {
        self.flows
            .lock()
            .expect("connection flows poisoned")
            .get(flow)
            .is_some_and(|f| f.state == FlowState::Waiting && f.until > Instant::now())
    }

    /// Where `person`'s sign-in `flow` is.
    pub fn flow(&self, person: &str, flow: &str) -> Option<FlowState> {
        let flows = self.flows.lock().expect("connection flows poisoned");
        let f = flows.get(flow).filter(|f| f.person == person)?;
        Some(match (&f.state, f.until > Instant::now()) {
            (FlowState::Waiting, false) => FlowState::Expired,
            (state, _) => state.clone(),
        })
    }

    /// Cancels `person`'s sign-in `flow`: nothing it later finds is kept.
    pub fn cancel(&self, person: &str, flow: &str) -> bool {
        let mut flows = self.flows.lock().expect("connection flows poisoned");
        match flows.get_mut(flow).filter(|f| f.person == person) {
            Some(f) => {
                if f.state == FlowState::Waiting {
                    f.state = FlowState::Cancelled;
                }
                true
            }
            None => false,
        }
    }

    /// Starts a sign-in for `id` at `level`.
    pub async fn start(
        self: &Arc<Self>,
        person: &str,
        id: &str,
        level: &str,
        agent: Option<&str>,
    ) -> Result<Started, Refusal> {
        let c = Self::connector(id)?;
        let level = self.level(level, agent)?;
        self.vault_at(c.tiers(matches!(level, Level::Agent(_)))[0], &level)?;
        match c.sign_in {
            SignIn::Github => self.start_github(person, c, level).await,
            SignIn::Google => self.start_google(person, level),
            SignIn::Fields => Err(Refusal::Invalid(format!(
                "{} is connected with a key",
                c.name
            ))),
        }
    }

    async fn start_github(
        self: &Arc<Self>,
        person: &str,
        c: &'static Connector,
        level: Level,
    ) -> Result<Started, Refusal> {
        let Some(client_id) = self.github_client_id.clone() else {
            return Err(Refusal::Unavailable(
                "Signing in with GitHub isn't set up on this Desk yet. Connect with a token instead."
                    .into(),
            ));
        };
        #[derive(Deserialize)]
        struct DeviceCode {
            device_code: String,
            user_code: String,
            verification_uri: String,
            expires_in: Option<u64>,
            interval: Option<u64>,
        }
        let response = self
            .services
            .http
            .post(format!("{}/login/device/code", self.services.github_web))
            .header("Accept", "application/json")
            .form(&[
                ("client_id", client_id.as_str()),
                ("scope", "repo read:org"),
            ])
            .send()
            .await
            .map_err(|_| Refusal::Unavailable("GitHub couldn't be reached".into()))?;
        let code: DeviceCode = response
            .json()
            .await
            .map_err(|_| Refusal::Unavailable("GitHub didn't start the sign-in".into()))?;
        let flow = self.insert_flow(person, level.clone());
        let expires_in = code.expires_in.unwrap_or(900);
        let this = Arc::clone(self);
        let (flow_id, person) = (flow.clone(), person.to_string());
        let interval =
            Duration::from_secs(code.interval.unwrap_or(5)).max(self.services.poll_floor);
        tokio::spawn(async move {
            let state = this
                .poll_github(
                    &flow_id,
                    &person,
                    c,
                    &level,
                    &client_id,
                    &code.device_code,
                    interval,
                )
                .await;
            this.settle(&flow_id, state);
        });
        Ok(Started {
            flow,
            url: None,
            user_code: Some(code.user_code),
            verification_uri: Some(code.verification_uri),
            expires_in,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn poll_github(
        &self,
        flow: &str,
        person: &str,
        c: &'static Connector,
        level: &Level,
        client_id: &str,
        device_code: &str,
        mut interval: Duration,
    ) -> FlowState {
        loop {
            tokio::time::sleep(interval).await;
            if !self.waiting(flow) {
                return FlowState::Cancelled;
            }
            let answer = self
                .services
                .http
                .post(format!(
                    "{}/login/oauth/access_token",
                    self.services.github_web
                ))
                .header("Accept", "application/json")
                .form(&[
                    ("client_id", client_id),
                    ("device_code", device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ])
                .send()
                .await;
            let Ok(answer) = answer else { continue };
            let v: Value = answer.json().await.unwrap_or_default();
            if let Some(token) = v.get("access_token").and_then(Value::as_str) {
                // Nothing is kept once the person cancelled.
                if !self.waiting(flow) {
                    return FlowState::Cancelled;
                }
                let fields = BTreeMap::from([("token".to_string(), token.to_string())]);
                return match self.services.github_user(token).await {
                    Ok(Found::Works { account }) => match self.keep(c, level, person, &fields) {
                        Ok(_) => {
                            let found = Found::Works {
                                account: account.clone(),
                            };
                            self.record(c, level, person, &found, true).await;
                            FlowState::Connected { account }
                        }
                        Err(_) => FlowState::Failed {
                            error: "Desk couldn't keep the sign-in".into(),
                        },
                    },
                    Ok(Found::Refused(e) | Found::Failed(e))
                    | Err(Found::Refused(e) | Found::Failed(e)) => FlowState::Failed {
                        error: scrub(&e, &fields),
                    },
                    Err(Found::Works { .. }) => FlowState::Failed {
                        error: "GitHub's answer couldn't be read".into(),
                    },
                };
            }
            match v.get("error").and_then(Value::as_str) {
                Some("authorization_pending") => {}
                Some("slow_down") => interval += Duration::from_secs(5),
                Some("expired_token") => return FlowState::Expired,
                Some("access_denied") => return FlowState::Cancelled,
                Some(other) => {
                    return FlowState::Failed {
                        error: format!("GitHub said {other}"),
                    }
                }
                None => {}
            }
        }
    }

    fn start_google(&self, person: &str, level: Level) -> Result<Started, Refusal> {
        use workforce_skills::adapters::google::oauth::{start_web_flow, OAuthClientCreds};
        use workforce_skills::adapters::google::web_flow::DeskFlow;
        let Some((client_id, client_secret, redirect)) = self.google_client() else {
            return Err(Refusal::Invalid(
                "Add your Google OAuth client first: its client ID and secret".into(),
            ));
        };
        let redirect_uri = redirect.unwrap_or_else(|| self.google_redirect.clone());
        let creds = OAuthClientCreds {
            client_id,
            client_secret,
            redirect_uri: Some(redirect_uri.clone()),
        };
        let web = start_web_flow(&creds, &redirect_uri);
        let agent = match &level {
            Level::All => None,
            Level::Agent(agent) => Some(agent.clone()),
        };
        let flow = self.insert_flow(person, level);
        crate::google_oauth_callback::google_web_flow_store().insert_flow(
            web.state,
            person.to_string(),
            catalog::GOOGLE_WORKSPACE.to_string(),
            web.verifier,
            Some(DeskFlow {
                flow: flow.clone(),
                agent,
                redirect_uri,
            }),
        );
        Ok(Started {
            flow,
            url: Some(web.authorization_url),
            user_code: None,
            verification_uri: None,
            expires_in: FLOW_TTL.as_secs(),
        })
    }

    /// Google came back without signing in: the flow is cancelled.
    pub fn google_cancelled(&self, flow: &str) {
        self.settle(flow, FlowState::Cancelled);
    }

    /// Google's sign-in failed before anything was kept.
    pub fn google_failed(&self, flow: &str, error: &str) {
        self.settle(
            flow,
            FlowState::Failed {
                error: error.to_string(),
            },
        );
    }

    /// Google signed `person` in for `flow`: keeps the tokens at the flow's
    /// tier, unless it was cancelled meanwhile.
    pub async fn google_signed_in(
        &self,
        flow: &str,
        person: &str,
        tokens: &workforce_skills::adapters::google::oauth::StoredTokens,
    ) -> Result<String, String> {
        let level = {
            let flows = self.flows.lock().expect("connection flows poisoned");
            match flows.get(flow) {
                Some(f) if f.person == person && f.state == FlowState::Waiting => f.level.clone(),
                _ => {
                    return Err(
                        "This sign-in was cancelled or has expired. Nothing was saved.".into(),
                    )
                }
            }
        };
        let c = catalog::find("google").expect("google is in the catalog");
        let tier = c.tiers(matches!(level, Level::Agent(_)))[0];
        let vault = match self.vault_at(tier, &level) {
            Ok(vault) => vault,
            Err(_) => {
                self.settle(
                    flow,
                    FlowState::Failed {
                        error: "Desk couldn't keep the sign-in".into(),
                    },
                );
                return Err("Desk couldn't keep the sign-in.".into());
            }
        };
        let store = crate::user_config::UserAwareVault::for_user(vault, person.to_string());
        if let Err(e) = workforce_skills::adapters::google::oauth::persist_tokens_at_user_scope(
            &store,
            person,
            catalog::GOOGLE_WORKSPACE,
            tokens,
        ) {
            tracing::error!(error = %format!("{e:#}"), "keeping a Google sign-in failed");
            self.settle(
                flow,
                FlowState::Failed {
                    error: "Desk couldn't keep the sign-in".into(),
                },
            );
            return Err("Desk couldn't keep the sign-in.".into());
        }
        let account = tokens.email.clone().unwrap_or_else(|| "Google".into());
        let found = Found::Works {
            account: account.clone(),
        };
        self.record(c, &level, person, &found, true).await;
        self.settle(
            flow,
            FlowState::Connected {
                account: account.clone(),
            },
        );
        Ok(account)
    }
}

/// Every other built-in adapter: an agent nobody has chosen connectors for
/// has these too, so a first choice keeps them.
pub fn other_adapters() -> Vec<&'static str> {
    crate::skill_loader::builtin_skill_catalog()
        .iter()
        .map(|m| m.tool_prefix)
        .filter(|p| catalog::find(p).is_none())
        .collect()
}

static INSTALLED: OnceLock<Arc<Connections>> = OnceLock::new();

/// The hub's Connections, once a `[desk]` hub mounted them.
pub fn installed() -> Option<Arc<Connections>> {
    INSTALLED.get().cloned()
}

/// The hub's agents, from its registry.
struct Registry(Arc<GatewayState>);

impl Agents for Registry {
    fn all(&self) -> Vec<Agent> {
        let mut agents: Vec<Agent> = self
            .0
            .registry
            .all_services()
            .iter()
            .map(|s| Agent {
                id: s.agent_id.as_str().to_string(),
                name: crate::desk_crew::name_of(s),
                vault: s.vault.clone(),
            })
            .collect();
        agents.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        agents
    }

    fn get(&self, id: &str) -> Option<Agent> {
        let s = self.0.registry.get(id)?;
        Some(Agent {
            id: s.agent_id.as_str().to_string(),
            name: crate::desk_crew::name_of(&s),
            vault: s.vault.clone(),
        })
    }
}

/// The hub's Google callback, on this computer.
fn google_redirect(listen: &str) -> String {
    let port = listen.rsplit_once(':').map_or("8080", |(_, port)| port);
    format!("http://127.0.0.1:{port}/api/auth/google/callback")
}

/// Connections for this `[desk]` hub, kept for the Google callback, and
/// its routes.
pub fn install(
    state: &Arc<GatewayState>,
    config: &workforce_common::settings::WorkforceConfig,
    desk: workforce_desk::Desk,
) -> Router {
    let connections = INSTALLED
        .get_or_init(|| {
            Arc::new(Connections::new(
                state.vault.clone(),
                Arc::new(Registry(Arc::clone(state))),
                Arc::new(desk),
                Services::live(),
                config.desk.github_client_id.clone(),
                google_redirect(&config.gateway.listen),
            ))
        })
        .clone();
    router(Arc::clone(state), connections)
}

struct Routes {
    state: Arc<GatewayState>,
    connections: Arc<Connections>,
}

type S = State<Arc<Routes>>;

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

#[derive(Deserialize)]
struct At {
    level: String,
    agent: Option<String>,
}

#[derive(Deserialize)]
struct ConnectBody {
    level: String,
    agent: Option<String>,
    #[serde(default)]
    fields: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct ClientBody {
    client_id: String,
    client_secret: String,
}

fn refusal(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

/// The person asking: signed in as themselves, holding this hub.
async fn person(
    routes: &Routes,
    headers: &HeaderMap,
    token: Option<&str>,
) -> Result<String, Response> {
    let caller = resolve_caller_context(&routes.state, headers, token).await;
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
            "Connections are a person's: ask as yourself, not as the hub",
        ));
    };
    if !crate::desk_floor::holds_the_hub(routes.state.identity_store.as_ref(), true, Some(&person))
    {
        return Err(refusal(
            StatusCode::FORBIDDEN,
            "Only the person whose Desk this is keeps its connections",
        ));
    }
    Ok(person)
}

macro_rules! person {
    ($r:expr, $h:expr, $q:expr) => {
        match person(&$r, &$h, $q.token.as_deref()).await {
            Ok(person) => person,
            Err(refused) => return refused,
        }
    };
}

/// Connections' routes over `connections`.
pub fn router(state: Arc<GatewayState>, connections: Arc<Connections>) -> Router {
    Router::new()
        .route("/api/desk/connections", get(list))
        .route(
            "/api/desk/connections/{connector}",
            post(connect).delete(disconnect),
        )
        .route("/api/desk/connections/{connector}/test", post(test))
        .route("/api/desk/connections/{connector}/client", post(client))
        .route("/api/desk/connections/{connector}/oauth/start", post(start))
        .route(
            "/api/desk/connection-flows/{flow}",
            get(flow).delete(cancel),
        )
        .with_state(Arc::new(Routes { state, connections }))
}

async fn list(State(r): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    let person = person!(r, h, q);
    Json(json!({
        "connectors": r.connections.list(&person).await,
        "adapters": other_adapters(),
    }))
    .into_response()
}

async fn connect(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(connector): Path<String>,
    Json(body): Json<ConnectBody>,
) -> Response {
    let person = person!(r, h, q);
    r.connections
        .connect(
            &person,
            &connector,
            &body.level,
            body.agent.as_deref(),
            body.fields,
        )
        .await
        .map(|c| Json(json!({ "connection": c })))
        .into_response()
}

async fn test(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(connector): Path<String>,
    Json(at): Json<At>,
) -> Response {
    let person = person!(r, h, q);
    r.connections
        .test(&person, &connector, &at.level, at.agent.as_deref())
        .await
        .map(|c| Json(json!({ "connection": c })))
        .into_response()
}

async fn disconnect(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(connector): Path<String>,
    Query(at): Query<At>,
) -> Response {
    let person = person!(r, h, q);
    r.connections
        .disconnect(&person, &connector, &at.level, at.agent.as_deref())
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

async fn client(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(connector): Path<String>,
    Json(body): Json<ClientBody>,
) -> Response {
    let person = person!(r, h, q);
    if connector != "google" {
        return Refusal::NotFound(format!("{connector} takes no OAuth client")).into_response();
    }
    r.connections
        .keep_google_client(&person, &body.client_id, &body.client_secret)
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

async fn start(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(connector): Path<String>,
    Json(at): Json<At>,
) -> Response {
    let person = person!(r, h, q);
    r.connections
        .start(&person, &connector, &at.level, at.agent.as_deref())
        .await
        .map(Json)
        .into_response()
}

async fn flow(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(flow): Path<String>,
) -> Response {
    let person = person!(r, h, q);
    match r.connections.flow(&person, &flow) {
        Some(state) => Json(state).into_response(),
        None => refusal(StatusCode::NOT_FOUND, "There's no such sign-in"),
    }
}

async fn cancel(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(flow): Path<String>,
) -> Response {
    let person = person!(r, h, q);
    match r.connections.cancel(&person, &flow) {
        true => StatusCode::NO_CONTENT.into_response(),
        false => refusal(StatusCode::NOT_FOUND, "There's no such sign-in"),
    }
}

//! Settings → Connections: the services a person connects for their local
//! agents, on a hub with `[desk]` only. A service may hold several
//! connections (two Jira sites, work and personal email), each a name the
//! person gave it over an instance of the service's adapter.
//!
//! - `GET /api/desk/connections`: every connector, its connections (each
//!   with its name, where it is kept, what its last check found, and which
//!   agents use it). Never a value. A connection kept before a service held
//!   several is moved to its adapter's `default` instance first.
//! - `POST /api/desk/connections/{connector}` `{name?, level, agent?,
//!   fields}`: tests the keys, then keeps them as a new connection at the
//!   level's vault tier. A failed Test keeps nothing.
//! - `POST …/{connector}/{connection}` `{fields}`: reconnects it with new
//!   keys; `PATCH` `{name}` renames it; `DELETE` removes its keys alone;
//!   `POST …/{connector}/{connection}/test` checks it again.
//! - `POST …/{connector}/oauth/start` `{name?, level, agent?}`: GitHub's
//!   device sign-in, or Google's or FreeAgent's sign-in page on the person's
//!   own app ([`app`]), for a new connection; `POST …/{connector}/app`
//!   `{client_id, client_secret}` keeps that app, and `POST
//!   …/{connector}/{connection}/approve` signs a connection in again,
//!   keeping its name and its agents' choice of it.
//! - `GET|DELETE /api/desk/connection-flows/{flow}`: a sign-in's progress,
//!   or cancelling it.
//! - `GET /api/desk/agents/{agent}/documents`: `{path}`, the agent's
//!   documents folder.
//! - `POST …/{connector}/test` `{level, agent?}` and `DELETE
//!   …/{connector}?level=&agent=` act on the level's one connection, for a
//!   Desk from before a service held several.
//!
//! "All my agents" is the vault's `GlobalPerUser` tier and "just this agent"
//! its `AgentPerUser` one. Slack's bot token is shared by its nature, so it
//! is kept at `Global` / `AgentSpecific`, and Slack holds one connection. A
//! key a person saved before Connections, at the bare tier, still counts.
//! Requests are the person's own, through their paired device, and only the
//! person whose hub this is may make them.

pub mod app;
pub mod approvals;
pub mod catalog;
pub mod check;
pub mod gh;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap};
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
use tracing::{info, warn};
use uuid::Uuid;
use workforce_common::settings::{VaultActor, VaultSource};
use workforce_desk::connections::{Checked, Level, Status};

use crate::gateway::{resolve_caller_context, AuthFailure, GatewayState};
use crate::vault::{Vault, VaultOp};
use catalog::{Connector, Field, SignIn, CATALOG, DEFAULT_INSTANCE};
use check::{Found, Services};

/// The largest value one field takes.
const MAX_VALUE: usize = 16 * 1024;
/// The longest name a connection takes.
const MAX_NAME: usize = 80;
/// How long a sign-in is waited on.
const FLOW_TTL: Duration = Duration::from_secs(15 * 60);

/// A local agent, as Connections sees it.
#[derive(Clone)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub vault: Option<Arc<Vault>>,
    /// Its `[skills]`, as its `config.toml` has it now.
    pub binding: Binding,
}

/// An agent's `[skills]` as "Used by" reads it: which connectors are on,
/// which use its own connection, and the connections it names. AC9a's
/// grammar, read raw here so the listing follows it on any hub version.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Binding {
    /// `None`: every connector, as before the key existed.
    pub enabled: Option<Vec<String>>,
    pub own: Vec<String>,
    /// Connectors, and single connections (`jira:globex`), that only read.
    pub read_only: Vec<String>,
    /// Connectors, and single connections (`freeagent:harbor`), that start
    /// read only and take writes.
    pub writable: Vec<String>,
    /// `[skills.connections]`: per connector, the instances it uses, the
    /// first its default.
    pub connections: BTreeMap<String, Vec<String>>,
}

impl Binding {
    /// `[skills]` of the `config.toml` in `workspace`.
    pub fn read(workspace: &std::path::Path) -> Self {
        let body = std::fs::read_to_string(
            workspace.join(workforce_common::config_files::WORKSPACE_CONFIG),
        )
        .unwrap_or_default();
        Self::parse(&body)
    }

    /// `[skills]` of a `config.toml` body; none where it doesn't parse.
    pub fn parse(body: &str) -> Self {
        let Ok(doc) = body.parse::<toml::Table>() else {
            return Self::default();
        };
        match workforce_common::connector_binding::of(doc.get("skills")) {
            Ok(b) => Self {
                enabled: b.enabled,
                own: b.own_credential,
                read_only: b.read_only,
                writable: b.writable,
                connections: b.connections,
            },
            Err(_) => Self::default(),
        }
    }

    fn is_on(&self, connector: &str) -> bool {
        self.enabled
            .as_ref()
            .is_none_or(|e| e.iter().any(|c| c == connector))
            && self
                .connections
                .get(connector)
                .is_none_or(|p| !p.is_empty())
    }

    /// The instances of `connector` it names, the first its default.
    pub fn picks(&self, connector: &str) -> Option<&[String]> {
        self.connections.get(connector).map(Vec::as_slice)
    }

    /// Whether instance `key` of `connector` only reads for it, as the hub
    /// enforces it.
    pub fn reads_only(&self, connector: &str, key: &str) -> bool {
        workforce_common::connector_binding::ConnectorBinding {
            read_only: self.read_only.clone(),
            writable: self.writable.clone(),
            ..Default::default()
        }
        .reads_only_instance(connector, key)
    }

    /// Whether the agent `agent` with this binding reaches instance `key`
    /// of `connector`, kept at `level`: one it names, when it names them;
    /// otherwise by the tiers its pin admits: none when the connector is
    /// off; only its own when it uses its own connection; only the
    /// person's when it names `enabled` and uses theirs; both when it
    /// names neither, as before the keys existed.
    fn reaches(&self, agent: &str, connector: &str, level: &Level, key: &str) -> bool {
        if !self.is_on(connector) {
            return false;
        }
        if matches!(level, Level::Agent(a) if a != agent) {
            return false;
        }
        if let Some(picks) = self.picks(connector) {
            return picks.iter().any(|p| p == key);
        }
        let own = self.own.iter().any(|o| o == connector);
        match level {
            Level::Agent(_) => own || self.enabled.is_none(),
            Level::All => !own,
        }
    }
}

/// The hub's agents.
#[async_trait]
pub trait Agents: Send + Sync {
    fn all(&self) -> Vec<Agent>;
    fn get(&self, id: &str) -> Option<Agent> {
        self.all().into_iter().find(|a| a.id == id)
    }
    /// Loads `connector` again for `agent`, as `person`, so an adapter that
    /// copied its keys when it loaded (Email) or holds them in memory
    /// (Slack) uses what is kept now.
    async fn reload(&self, _agent: &str, _connector: &str, _person: &str) {}
    /// Keeps `agent`'s daily watch of `person`'s bank approvals
    /// ([`approvals`]) while `on`, and removes it once not.
    async fn watch_approvals(&self, _agent: &str, _person: &str, _on: bool) {}
    /// Makes `agent` use the connections `choice` names, in its `[skills]`,
    /// through the hub's one write of its `config.toml`, as `person`. What
    /// is wrong, in words.
    async fn choose(
        &self,
        _agent: &str,
        _person: &str,
        _choice: &picks::Choice,
    ) -> Result<(), String> {
        Err("Desk can't change this agent's connections here.".into())
    }
}

/// Where what a check found is kept.
#[async_trait]
pub trait Statuses: Send + Sync {
    async fn all(&self, person: &str) -> eyre::Result<Vec<Checked>>;
    async fn put(&self, checked: &Checked, connected: bool) -> eyre::Result<()>;
    async fn forget(
        &self,
        connector: &str,
        level: &Level,
        instance: &str,
        person: &str,
    ) -> eyre::Result<()>;
    /// A connection moved from instance `from` to `to`: its checks go with
    /// it.
    async fn moved(
        &self,
        connector: &str,
        level: &Level,
        person: &str,
        from: &str,
        to: &str,
    ) -> eyre::Result<()>;
}

#[async_trait]
impl Statuses for workforce_desk::Desk {
    async fn all(&self, person: &str) -> eyre::Result<Vec<Checked>> {
        self.store().connections(person).await
    }
    async fn put(&self, checked: &Checked, connected: bool) -> eyre::Result<()> {
        self.store().put_connection(checked, connected).await
    }
    async fn forget(
        &self,
        connector: &str,
        level: &Level,
        instance: &str,
        person: &str,
    ) -> eyre::Result<()> {
        self.store()
            .drop_connection(connector, level, instance, person)
            .await
    }
    async fn moved(
        &self,
        connector: &str,
        level: &Level,
        person: &str,
        from: &str,
        to: &str,
    ) -> eyre::Result<()> {
        self.store()
            .move_connection(connector, level, person, from, to)
            .await
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

/// One connection: a service's instance, kept at a level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conn {
    pub level: Level,
    /// The adapter instance (`acme`); `default` for Slack's one.
    pub key: String,
}

impl Conn {
    /// The id Desk names it by, and never shows: `all~~acme`,
    /// `agent~allan~acme`.
    pub fn id(&self) -> String {
        format!(
            "{}~{}~{}",
            self.level.as_str(),
            self.level.agent(),
            self.key
        )
    }
}

/// An agent a connection reaches, and through whose keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsedBy {
    pub agent: String,
    pub name: String,
    /// `mine` (the person's, for all their agents) or `own` (just this
    /// agent's).
    pub connection: &'static str,
}

/// A connection as the page shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionView {
    /// What Desk names it by in a request; never shown.
    pub id: String,
    /// The name the person gave it: "Acme", "Work".
    pub name: String,
    /// Where it is, for a service with a site: `acme.atlassian.net`.
    pub site: Option<String>,
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
    /// When the person's approval at the service lapses, about, for a
    /// service whose approval does (a bank's).
    pub approval: Option<ApprovalView>,
    /// The vault tier it is kept at.
    pub source: VaultSource,
    /// The agents it reaches.
    pub used_by: Vec<UsedBy>,
    /// Whether Desk offers to approve it again: it signs in on the
    /// person's app, and its sign-in lapsed or was on an earlier app.
    pub approve_again: bool,
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
    /// person's own app is kept and names this hub.
    pub sign_in_ready: bool,
    /// For a service signed in to on the person's own app: `ready`,
    /// `missing` (none kept yet) or `moved` (FreeAgent's names an older
    /// address of this hub).
    pub app: Option<app::AppState>,
    /// Whether it holds more than one connection (Slack holds one).
    pub several: bool,
    /// What a new connection's name should be, as the form says.
    pub name_hint: &'static str,
    /// How its approval is renewed, for one whose approval lapses.
    pub renewal: Option<catalog::Renewal>,
    /// It only ever reads.
    pub reads_only: bool,
    pub connections: Vec<ConnectionView>,
    /// Every agent it reaches, through any of its connections.
    pub used_by: Vec<UsedBy>,
}

/// When a connection's approval lapses, by the UTC calendar: an estimate,
/// as the service doesn't say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalView {
    pub until: chrono::NaiveDate,
    pub days_left: i64,
    /// It lapses within the week, or has.
    pub due: bool,
    pub estimated: bool,
}

impl From<workforce_skills::adapters::bank::model::Approval> for ApprovalView {
    fn from(a: workforce_skills::adapters::bank::model::Approval) -> Self {
        Self {
            until: a.until,
            days_left: a.days_left,
            due: a.due,
            estimated: a.estimated,
        }
    }
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

/// What a disconnect left.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Disconnected {
    /// The tier a key of this connection is still kept at, if any.
    pub still_kept: Option<VaultSource>,
    /// What to tell the person about it.
    pub note: Option<String>,
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
    /// The connector it signs in to.
    connector: &'static str,
    level: Level,
    /// The instance it keeps; GitHub's is its account's login when the
    /// person named none.
    key: Option<String>,
    /// The name the person gave it, if any.
    name: Option<String>,
    /// It signs connection `key` in again: its name stays.
    again: bool,
    state: FlowState,
    until: Instant,
}

/// A sign-in still waited on, as its flow keeps it.
struct WaitingFlow {
    person: String,
    level: Level,
    key: Option<String>,
    name: Option<String>,
    again: bool,
}

/// Connections, over the hub's vaults and Desk's store.
pub struct Connections {
    pub global: Option<Arc<Vault>>,
    pub agents: Arc<dyn Agents>,
    pub statuses: Arc<dyn Statuses>,
    pub services: Services,
    /// Driftwerk's GitHub OAuth app, for the device sign-in.
    pub github_client_id: Option<String>,
    /// The gateway's listen address: the port a sign-in's loopback
    /// redirect names.
    pub gateway: String,
    flows: Mutex<HashMap<String, Flow>>,
    /// Sign-ins on the person's own app, by their `state`.
    pending: Mutex<HashMap<String, app::Pending>>,
    /// Held while flat connections are moved, so two requests never move
    /// one twice.
    migrating: tokio::sync::Mutex<()>,
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

/// The instance key a name makes: lower case letters, digits and dashes.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Whether `key` is an instance key as [`slug`] makes one: lower case
/// letters, digits and dashes, at most 64 long. Any other (a key written by
/// hand in the vault editor) isn't one Connections names or lists.
pub fn is_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A site as a name: `acme.atlassian.net` and `https://acme.atlassian.net`
/// are "Acme".
fn site_name(site: &str) -> Option<String> {
    let host = site
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split(['/', ':'])
        .next()?;
    let first = host.split('.').next().filter(|f| !f.is_empty())?;
    let mut chars = first.chars();
    let head = chars.next()?;
    Some(head.to_uppercase().chain(chars).collect())
}

/// What a connection is called when the person named it nothing: its
/// account, else its site, else the service.
fn name_of(c: &Connector, account: Option<&str>, site: Option<&str>) -> String {
    account
        .filter(|a| !a.trim().is_empty())
        .map(str::to_string)
        .or_else(|| site.and_then(site_name))
        .unwrap_or_else(|| c.name.to_string())
}

impl Connections {
    pub fn new(
        global: Option<Arc<Vault>>,
        agents: Arc<dyn Agents>,
        statuses: Arc<dyn Statuses>,
        services: Services,
        github_client_id: Option<String>,
        gateway: String,
    ) -> Self {
        Self {
            global,
            agents,
            statuses,
            services,
            github_client_id: github_client_id.filter(|id| !id.trim().is_empty()),
            gateway,
            flows: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            migrating: tokio::sync::Mutex::new(()),
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

    /// The connection `id` names (`all~~acme`), as the page sent it back.
    fn conn(&self, id: &str) -> Result<Conn, Refusal> {
        let gone = || Refusal::NotFound("That connection isn't there any more".into());
        let mut parts = id.splitn(3, '~');
        let (Some(level), Some(agent), Some(key)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(gone());
        };
        if !is_key(key) {
            return Err(gone());
        }
        let agent = (!agent.is_empty()).then_some(agent);
        Ok(Conn {
            level: self.level(level, agent)?,
            key: key.to_string(),
        })
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

    /// The tiers of `level`, the one written first, each once.
    fn tiers(c: &Connector, level: &Level) -> Vec<VaultSource> {
        let mut tiers = c.tiers(matches!(level, Level::Agent(_))).to_vec();
        tiers.dedup();
        tiers
    }

    /// Every level a connection may be kept at: all the person's agents,
    /// then each agent's own.
    fn levels(&self) -> Vec<Level> {
        std::iter::once(Level::All)
            .chain(self.agents.all().into_iter().map(|a| Level::Agent(a.id)))
            .collect()
    }

    /// The tier instance `key` of `c` is kept at, at `level`, if it is
    /// connected.
    fn held_at(
        &self,
        c: &Connector,
        level: &Level,
        key: &str,
        person: &str,
    ) -> Option<VaultSource> {
        Self::tiers(c, level).into_iter().find(|tier| {
            self.vault_at(*tier, level)
                .ok()
                .is_some_and(|v| v.get(&key_at(*tier, person, &c.primary_at(key))).is_some())
        })
    }

    /// Every connection of `c` kept at `level`, with its tier, by key.
    fn instances(&self, c: &Connector, level: &Level, person: &str) -> Vec<(String, VaultSource)> {
        let Some(prefix) = c.prefix else {
            return self
                .held_at(c, level, DEFAULT_INSTANCE, person)
                .map(|tier| (DEFAULT_INSTANCE.to_string(), tier))
                .into_iter()
                .collect();
        };
        let mut out: Vec<(String, VaultSource)> = Vec::new();
        for tier in Self::tiers(c, level) {
            let Ok(vault) = self.vault_at(tier, level) else {
                continue;
            };
            let at = key_at(tier, person, prefix);
            let keys: BTreeSet<String> = vault
                .list(Some(&at))
                .into_iter()
                .filter_map(|path| {
                    let rest = path.strip_prefix(&at)?;
                    let (key, _) = rest.split_once('/')?;
                    Some(key.to_string())
                })
                .filter(|key| is_key(key))
                .collect();
            for key in keys {
                let held = vault
                    .get(&key_at(tier, person, &c.primary_at(&key)))
                    .is_some();
                if held && !out.iter().any(|(k, _)| *k == key) {
                    out.push((key, tier));
                }
            }
        }
        out
    }

    /// Every instance key of `c` the person holds anywhere.
    /// The keys `person`'s connections of `c` hold, and those a sign-in of
    /// theirs still waiting will keep: two started together never share one.
    fn keys_in_use(&self, c: &Connector, person: &str) -> BTreeSet<String> {
        let mut keys: BTreeSet<String> = self
            .levels()
            .iter()
            .flat_map(|level| self.instances(c, level, person))
            .map(|(key, _)| key)
            .collect();
        let now = Instant::now();
        let flows = self.flows.lock().expect("connection flows poisoned");
        keys.extend(
            flows
                .values()
                .filter(|f| {
                    f.person == person
                        && f.connector == c.id
                        && f.until > now
                        && matches!(f.state, FlowState::Waiting)
                })
                .filter_map(|f| f.key.clone()),
        );
        keys
    }

    /// A key for a new connection of `c` named `name`, unique among the
    /// person's: a connection's name is its key's first spelling, and the
    /// key never changes after.
    fn new_key(&self, c: &Connector, person: &str, name: &str) -> String {
        if !c.named() {
            return DEFAULT_INSTANCE.to_string();
        }
        let taken = self.keys_in_use(c, person);
        let base = match slug(name) {
            s if s.is_empty() => c.id.to_string(),
            s => s,
        };
        (1..)
            .map(|n| match n {
                1 => base.clone(),
                n => format!("{base}-{n}"),
            })
            .find(|k| !taken.contains(k))
            .expect("some key is free")
    }

    /// The kept fields and settings of instance `key` at `tier`.
    fn kept(
        &self,
        c: &Connector,
        level: &Level,
        key: &str,
        tier: VaultSource,
        person: &str,
    ) -> BTreeMap<String, String> {
        let Ok(vault) = self.vault_at(tier, level) else {
            return BTreeMap::new();
        };
        c.fields
            .iter()
            .map(|f| (f.name, c.field_at(f, key)))
            .chain(
                c.extras
                    .iter()
                    .map(|&extra| (extra.1, c.extra_at(extra, key))),
            )
            .filter_map(|(name, path)| {
                vault
                    .get(&key_at(tier, person, &path))
                    .map(|v| (name.to_string(), v))
            })
            .collect()
    }

    /// What a connection is kept as at `tier`: its name and its site.
    fn named_at(
        &self,
        c: &Connector,
        level: &Level,
        key: &str,
        tier: VaultSource,
        person: &str,
    ) -> (Option<String>, Option<String>) {
        let Ok(vault) = self.vault_at(tier, level) else {
            return (None, None);
        };
        let name = c
            .label_at(key)
            .and_then(|p| vault.get(&key_at(tier, person, &p)));
        let site = c
            .site
            .and_then(|s| c.fields.iter().find(|f| f.name == s))
            .and_then(|f| vault.get(&key_at(tier, person, &c.field_at(f, key))));
        (name, site)
    }

    fn view(
        &self,
        c: &Connector,
        conn: &Conn,
        tier: VaultSource,
        person: &str,
        checked: Option<&Checked>,
        agents: &[Agent],
    ) -> ConnectionView {
        let agent = match &conn.level {
            Level::All => None,
            Level::Agent(agent) => Some(agent.clone()),
        };
        let (name, site) = self.named_at(c, &conn.level, &conn.key, tier, person);
        let account = checked.and_then(|k| k.account.clone());
        let used_by = agents
            .iter()
            .filter(|a| a.binding.reaches(&a.id, c.id, &conn.level, &conn.key))
            .map(|a| UsedBy {
                agent: a.id.clone(),
                name: a.name.clone(),
                connection: match conn.level {
                    Level::All => "mine",
                    Level::Agent(_) => "own",
                },
            })
            .collect();
        ConnectionView {
            id: conn.id(),
            name: name.unwrap_or_else(|| name_of(c, account.as_deref(), site.as_deref())),
            site,
            level: conn.level.as_str(),
            agent_name: agent
                .as_deref()
                .and_then(|a| agents.iter().find(|x| x.id == a))
                .map(|a| a.name.clone()),
            agent,
            account,
            status: checked.map_or(Status::Connected, |k| k.status),
            detail: checked.and_then(|k| k.detail.clone()),
            connected_at: checked.map(|k| k.connected_at),
            checked_at: checked.and_then(|k| k.checked_at),
            approval: self.approval(c, conn, tier, person),
            source: tier,
            used_by,
            approve_again: c.sign_in == SignIn::App
                && checked.is_some_and(|k| k.status == Status::NeedsReconnect),
        }
    }

    fn sign_in_ready(&self, c: &Connector) -> bool {
        match c.sign_in {
            SignIn::Fields => true,
            SignIn::Github => self.github_client_id.is_some(),
            SignIn::App => self.app_state(c) == Some(app::AppState::Ready),
        }
    }

    /// Moves every flat connection of `person`'s (`skills/jira/api_token`)
    /// to its adapter's instance, at the tier it is kept at, in one write
    /// each, named after its account or site; and names an instance kept
    /// without one. Runs again harmlessly: what has moved isn't flat.
    pub async fn migrate(&self, person: &str) {
        let _one = self.migrating.lock().await;
        let checks = self.statuses.all(person).await.unwrap_or_default();
        for c in CATALOG.iter().filter(|c| c.named()) {
            for level in self.levels() {
                let checked = checks.iter().find(|k| {
                    k.connector == c.id && k.level == level && k.instance == DEFAULT_INSTANCE
                });
                let mut moved = false;
                for tier in Self::tiers(c, &level) {
                    let Ok(vault) = self.vault_at(tier, &level) else {
                        continue;
                    };
                    // The level's one check was of the connection its first
                    // tier held: only the first to move is named from it and
                    // takes it; one beside it (`shared`) has none yet.
                    let checked = checked.filter(|_| !moved);
                    match self.migrate_at(c, &level, tier, &vault, person, checked) {
                        Ok(Some(key)) => {
                            let first = !moved;
                            moved = true;
                            if first && key != DEFAULT_INSTANCE {
                                if let Err(e) = self
                                    .statuses
                                    .moved(c.id, &level, person, DEFAULT_INSTANCE, &key)
                                    .await
                                {
                                    warn!(connector = c.id, error = %format!("{e:#}"), "a moved connection's check stayed behind");
                                }
                            }
                            if let Level::Agent(agent) = &level {
                                self.keep_own(c, agent, &key, person).await;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            warn!(connector = c.id, tier = ?tier, error = %format!("{e:#}"), "a flat connection wasn't moved; it stays as it was");
                        }
                    }
                }
                if moved {
                    self.reload(c, &level, person).await;
                }
            }
        }
    }

    /// An agent's own connection kept before stays the one it uses once it
    /// is named, so nothing switches to the person's account under it: an
    /// agent that read its own connection (one it named in `own_credential`,
    /// or any, when it had chosen no connectors, since its own shadowed the
    /// person's) now names it in `[skills.connections]`, as its default.
    /// One that read only the person's, or has chosen its connections
    /// already, is left as it is.
    async fn keep_own(&self, c: &Connector, agent: &str, key: &str, person: &str) {
        let Some(a) = self.agents.get(agent) else {
            return;
        };
        let b = &a.binding;
        let used_own = b.own.iter().any(|o| o == c.id) || b.enabled.is_none();
        if b.picks(c.id).is_some() || !b.is_on(c.id) || !used_own {
            return;
        }
        let choice = picks::Choice {
            service: c.id,
            picked: vec![(key.to_string(), !b.reads_only(c.id, key))],
            default: Some(key.to_string()),
        };
        if let Err(e) = self.agents.choose(agent, person, &choice).await {
            warn!(agent, connector = c.id, error = %e, "an agent's own connection wasn't kept as the one it uses");
        }
    }

    /// [`Self::migrate`] of `c` at one tier: the instance it moved to or
    /// named, if anything changed.
    ///
    /// It becomes the adapters' `default` (GitHub's too),
    /// unless another level holds that already (the person's connection for
    /// all their agents, migrated first): it then takes a key of its own,
    /// from its account, else the service, so the two stay two connections
    /// an agent can pick one by one.
    ///
    /// Every field and setting moves (its keys, its address, the settings
    /// its adapter reads beside them), one by one, and none is written over:
    /// one the instance holds already stays, the flat one with it beside it
    /// if they differ (said in the log), gone if they are the same. So a
    /// flat key written after the move (by an older tool, say) joins the
    /// connection it belongs to, never replacing what it holds.
    fn migrate_at(
        &self,
        c: &Connector,
        level: &Level,
        tier: VaultSource,
        vault: &Vault,
        person: &str,
        checked: Option<&Checked>,
    ) -> eyre::Result<Option<String>> {
        let actor = VaultActor::User(person);
        // Email and Google always kept theirs at `default`, the instance
        // path: unnamed, it is one kept before Connections named them.
        let native = c.primary == c.primary_at(DEFAULT_INSTANCE);
        // Where each field and setting is kept now, and its name in an
        // instance.
        let pairs: Vec<(String, &str)> = c
            .fields
            .iter()
            .map(|f| match native {
                true => (c.at(DEFAULT_INSTANCE, f.sub), f.sub),
                false => (f.path.to_string(), f.sub),
            })
            .chain(c.extras.iter().map(|(flat, sub)| (flat.to_string(), *sub)))
            .collect();
        let present: Vec<(String, &str, String)> = pairs
            .into_iter()
            .filter_map(|(from, sub)| {
                let at = key_at(tier, person, &from);
                vault.get(&at).map(|v| (at, sub, v))
            })
            .collect();
        if present.is_empty() {
            return Ok(None);
        }
        let has_key = vault.get(&key_at(tier, person, c.primary)).is_some();
        let default_label = key_at(
            tier,
            person,
            &c.label_at(DEFAULT_INSTANCE).expect("a named connector"),
        );
        if native && (!has_key || vault.get(&default_label).is_some()) {
            return Ok(None);
        }
        let account = checked.and_then(|k| k.account.clone()).or_else(|| {
            native
                .then(|| {
                    ["username", "email"].iter().find_map(|sub| {
                        vault.get(&key_at(tier, person, &c.at(DEFAULT_INSTANCE, sub)))
                    })
                })
                .flatten()
        });
        let site = c.site.filter(|_| !native).and_then(|s| {
            c.fields
                .iter()
                .find(|f| f.name == s)
                .and_then(|f| vault.get(&key_at(tier, person, f.path)))
        });
        let name = name_of(c, account.as_deref(), site.as_deref());
        // GitHub's too: its flat token reached every owner, and `default` is
        // the one instance git, Claude Code and benches read for any owner
        // (`resolve_github_pat`), as the adapter's flat mode named it.
        let mut key = DEFAULT_INSTANCE.to_string();
        // The person's connection for all their agents keeps its key.
        let taken = *level != Level::All
            && self
                .levels()
                .iter()
                .filter(|l| *l != level)
                .any(|l| self.instances(c, l, person).iter().any(|(k, _)| *k == key));
        // Kept at both of this level's tiers (the person's own, and a bare
        // one beside it): the person's own keeps its key, and the bare one
        // takes `shared`, so both show.
        let beside = self
            .instances(c, level, person)
            .iter()
            .any(|(k, t)| *k == key && *t != tier);
        if beside {
            key = self.new_key(c, person, "shared");
        } else if taken {
            key = self.new_key(c, person, &name);
        }
        // A flat GitHub token reaches every owner; under any key but
        // `default` git, Claude Code and benches would read it for none
        // (`resolve_github_pat`). One that can't be `default` stays flat, as
        // it works today.
        if c.id == "github" && key != DEFAULT_INSTANCE {
            tracing::debug!(
                tier = ?tier,
                "a flat GitHub token stays flat: beside another connection it can't be the default git reads"
            );
            return Ok(None);
        }
        let label = key_at(tier, person, &c.label_at(&key).expect("a named connector"));
        // Email's or Google's own `default`: it only needs a name.
        if native && key == DEFAULT_INSTANCE {
            vault.apply(
                &[VaultOp::Set {
                    path: &label,
                    value: &name,
                }],
                actor,
            )?;
            return Ok(Some(key));
        }
        let target_has_key = vault
            .get(&key_at(tier, person, &c.primary_at(&key)))
            .is_some();
        // Settings with no key of their own, and no connection to join:
        // left as they are.
        if !has_key && !target_has_key {
            tracing::debug!(
                connector = c.id,
                "flat settings with no connection to join stay"
            );
            return Ok(None);
        }
        // Beside a connection that has its key, a flat field joins it only
        // when it came with that same key: a site of one account never pairs
        // with another's key.
        if target_has_key {
            let flat_key = vault.get(&key_at(tier, person, c.primary));
            let held = vault.get(&key_at(tier, person, &c.primary_at(&key)));
            if flat_key.is_none() || flat_key != held {
                warn!(
                    connector = c.id,
                    instance = %key,
                    "flat settings stay beside a connection: they didn't come with its key"
                );
                return Ok(None);
            }
        }
        let mut sets: Vec<(String, String)> = Vec::new();
        let mut deletes: Vec<String> = Vec::new();
        for (from, sub, value) in &present {
            let to = key_at(tier, person, &c.at(&key, sub));
            match vault.get(&to) {
                None => {
                    sets.push((to, value.clone()));
                    deletes.push(from.clone());
                }
                Some(kept) if kept == *value => deletes.push(from.clone()),
                Some(_) => warn!(
                    connector = c.id,
                    instance = %key,
                    field = sub,
                    "a flat setting differs from the connection's own and stays beside it; the connection's is used"
                ),
            }
        }
        if vault.get(&label).is_none() {
            sets.push((label.clone(), name.clone()));
        }
        if sets.is_empty() && deletes.is_empty() {
            return Ok(None);
        }
        let mut ops: Vec<VaultOp<'_>> = sets
            .iter()
            .map(|(path, value)| VaultOp::Set { path, value })
            .collect();
        ops.extend(deletes.iter().map(|path| VaultOp::Delete { path }));
        vault.apply(&ops, actor)?;
        info!(connector = c.id, instance = %key, tier = ?tier, moved = deletes.len(), "a flat connection moved to a named one");
        Ok(Some(key))
    }

    /// Every connector, as `person` has connected it.
    pub async fn list(&self, person: &str) -> Vec<ConnectorView> {
        self.migrate(person).await;
        let checks = self.statuses.all(person).await.unwrap_or_else(|e| {
            warn!(error = %format!("{e:#}"), "connection checks unreadable; showing keys only");
            Vec::new()
        });
        let agents = self.agents.all();
        let levels = self.levels();
        CATALOG
            .iter()
            .map(|c| {
                let mut connections = Vec::new();
                for level in &levels {
                    for (key, tier) in self.instances(c, level, person) {
                        let checked = checks.iter().find(|k| {
                            k.connector == c.id && &k.level == level && k.instance == key
                        });
                        let conn = Conn {
                            level: level.clone(),
                            key,
                        };
                        connections.push(self.view(c, &conn, tier, person, checked, &agents));
                    }
                }
                let mut used_by: Vec<UsedBy> = Vec::new();
                for u in connections.iter().flat_map(|x| x.used_by.iter()) {
                    match used_by.iter_mut().find(|x| x.agent == u.agent) {
                        // Its own connection wins, as its pin reads it first.
                        Some(x) if u.connection == "own" => x.connection = "own",
                        Some(_) => {}
                        None => used_by.push(u.clone()),
                    }
                }
                ConnectorView {
                    id: c.id,
                    name: c.name,
                    about: c.about,
                    sign_in: c.sign_in,
                    fields: c.fields,
                    sign_in_ready: self.sign_in_ready(c),
                    app: self.app_state(c),
                    several: c.named(),
                    name_hint: c.name_hint,
                    renewal: c.renewal,
                    reads_only: c.reads_only,
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

    /// A name as given: trimmed, not empty, not too long.
    fn name(c: &Connector, name: Option<&str>) -> Result<Option<String>, Refusal> {
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            return Ok(None);
        };
        if name.chars().count() > MAX_NAME {
            return Err(Refusal::Invalid(format!(
                "Give the {} connection a shorter name",
                c.name
            )));
        }
        if name.chars().any(char::is_control) {
            return Err(Refusal::Invalid("A name is one line of words".into()));
        }
        Ok(Some(name.to_string()))
    }

    /// Keeps `fields` as instance `key` at the level's tier: every one
    /// given, none of the optional ones left out, and its name where it is
    /// given.
    fn keep(
        &self,
        c: &Connector,
        level: &Level,
        key: &str,
        person: &str,
        fields: &BTreeMap<String, String>,
        name: Option<&str>,
    ) -> Result<VaultSource, Refusal> {
        let tier = Self::tiers(c, level)[0];
        let vault = self.vault_at(tier, level)?;
        // Its approval was given when it was first kept, to make its key: a
        // setup run again or a new key moves nothing.
        let now = Utc::now().to_rfc3339();
        let mut keys: Vec<(String, Option<&str>)> = c
            .fields
            .iter()
            .map(|f| {
                (
                    key_at(tier, person, &c.field_at(f, key)),
                    fields.get(f.name).map(String::as_str),
                )
            })
            .collect();
        if let (Some(label), Some(name)) = (c.label_at(key), name) {
            keys.push((key_at(tier, person, &label), Some(name)));
        }
        if let Some(at) = Self::consented_path(c, key)
            .map(|at| key_at(tier, person, &at))
            .filter(|at| vault.get(at).is_none())
        {
            keys.push((at, Some(&now)));
        }
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
        conn: &Conn,
        person: &str,
        found: &Found,
        connected: bool,
    ) {
        self.record_noted(c, conn, person, found, connected, None)
            .await
    }

    /// [`Self::record`], with what a working connection lacks said beside
    /// it.
    async fn record_noted(
        &self,
        c: &Connector,
        conn: &Conn,
        person: &str,
        found: &Found,
        connected: bool,
        note: Option<String>,
    ) {
        let (status, account, detail) = match found {
            Found::Works { account } => (Status::Connected, Some(account.clone()), note),
            Found::Refused(e) => (Status::NeedsReconnect, None, Some(e.clone())),
            Found::Failed(e) => (Status::Failing, None, Some(e.clone())),
        };
        let now = Utc::now();
        let checked = Checked {
            connector: c.id.to_string(),
            level: conn.level.clone(),
            instance: conn.key.clone(),
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

    /// Connection `conn` of `c` as the page shows it.
    pub async fn shown(
        &self,
        c: &Connector,
        conn: &Conn,
        person: &str,
    ) -> Result<ConnectionView, Refusal> {
        let tier = self
            .held_at(c, &conn.level, &conn.key, person)
            .ok_or_else(|| {
                Refusal::NotFound(format!("That {} connection isn't there any more", c.name))
            })?;
        let checks = self.statuses.all(person).await.unwrap_or_default();
        let checked = checks
            .iter()
            .find(|k| k.connector == c.id && k.level == conn.level && k.instance == conn.key);
        Ok(self.view(c, conn, tier, person, checked, &self.agents.all()))
    }

    /// Runs `c`'s Test on `fields`, its words scrubbed of them.
    async fn check(&self, c: &Connector, person: &str, fields: &BTreeMap<String, String>) -> Found {
        match self.services.check(c.id, person, fields).await {
            Found::Works { account } => Found::Works { account },
            Found::Refused(e) => Found::Refused(scrub(&e, fields)),
            Found::Failed(e) => Found::Failed(scrub(&e, fields)),
        }
    }

    fn failed(found: Found) -> Refusal {
        match found {
            Found::Refused(error) => Refusal::TestFailed {
                error,
                status: Status::NeedsReconnect,
            },
            Found::Failed(error) => Refusal::TestFailed {
                error,
                status: Status::Failing,
            },
            Found::Works { .. } => Refusal::Internal(eyre::eyre!("a working key failed")),
        }
    }

    /// Tests `fields`, then keeps them as a new connection named `name`
    /// (its account, if the person named it nothing). A failed Test keeps
    /// nothing. Slack, which holds one connection, is connected again.
    pub async fn connect(
        &self,
        person: &str,
        id: &str,
        name: Option<&str>,
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
        let name = Self::name(c, name)?;
        let fields = Self::fields(c, given)?;
        // The tier must be reachable before anything is sent anywhere.
        self.vault_at(Self::tiers(c, &level)[0], &level)?;
        // A flat connection still kept would be hidden by a named one.
        self.migrate(person).await;
        let account = match self.check(c, person, &fields).await {
            Found::Works { account } => account,
            other => return Err(Self::failed(other)),
        };
        let site = c.site.and_then(|s| fields.get(s)).map(String::as_str);
        let name = name.unwrap_or_else(|| name_of(c, Some(&account), site));
        let key = match self.keys_in_use(c, person).is_empty() {
            // A first connection is the adapters' `default`, as a flat one
            // was: for GitHub, the token git reaches any owner with.
            true => DEFAULT_INSTANCE.to_string(),
            false => self.new_key(c, person, &name),
        };
        let conn = Conn { level, key };
        self.keep(c, &conn.level, &conn.key, person, &fields, Some(&name))?;
        self.record(c, &conn, person, &Found::Works { account }, true)
            .await;
        self.reload(c, &conn.level, person).await;
        self.shown(c, &conn, person).await
    }

    /// Tests new `fields` for connection `conn`, then keeps them over its
    /// old ones. A failed Test keeps the old ones as they were.
    pub async fn reconnect(
        &self,
        person: &str,
        id: &str,
        conn: &str,
        given: BTreeMap<String, String>,
    ) -> Result<ConnectionView, Refusal> {
        let c = Self::connector(id)?;
        let conn = self.conn(conn)?;
        if c.fields.is_empty() {
            return Err(Refusal::Invalid(format!(
                "{} is connected again by signing in",
                c.name
            )));
        }
        self.held_at(c, &conn.level, &conn.key, person)
            .ok_or_else(|| {
                Refusal::NotFound(format!("That {} connection isn't there any more", c.name))
            })?;
        let fields = Self::fields(c, given)?;
        let found = self.check(c, person, &fields).await;
        let Found::Works { .. } = &found else {
            return Err(Self::failed(found));
        };
        self.keep(c, &conn.level, &conn.key, person, &fields, None)?;
        self.record(c, &conn, person, &found, true).await;
        self.reload(c, &conn.level, person).await;
        self.shown(c, &conn, person).await
    }

    /// Gives connection `conn` a new name. Its instance stays as it was, so
    /// what refers to it (an agent's choice, an environment) still does.
    pub async fn rename(
        &self,
        person: &str,
        id: &str,
        conn: &str,
        name: &str,
    ) -> Result<ConnectionView, Refusal> {
        let c = Self::connector(id)?;
        let conn = self.conn(conn)?;
        let Some(label) = c.label_at(&conn.key) else {
            return Err(Refusal::Invalid(format!(
                "{} has one connection, which takes no name",
                c.name
            )));
        };
        let name =
            Self::name(c, Some(name))?.ok_or_else(|| Refusal::Invalid("Give it a name".into()))?;
        let tier = self
            .held_at(c, &conn.level, &conn.key, person)
            .ok_or_else(|| {
                Refusal::NotFound(format!("That {} connection isn't there any more", c.name))
            })?;
        let vault = self.vault_at(tier, &conn.level)?;
        let path = key_at(tier, person, &label);
        vault
            .apply(
                &[VaultOp::Set {
                    path: &path,
                    value: &name,
                }],
                VaultActor::User(person),
            )
            .map_err(Refusal::Internal)?;
        // Its agents are told it by its new name.
        self.reload(c, &conn.level, person).await;
        self.shown(c, &conn, person).await
    }

    /// Where `c`'s instance `key` keeps when it was approved, for a service
    /// whose approval lapses.
    fn consented_path(c: &Connector, key: &str) -> Option<String> {
        c.renewal?;
        c.extras
            .iter()
            .find(|(_, sub)| *sub == catalog::CONSENTED_AT)
            .map(|&extra| c.extra_at(extra, key))
    }

    /// When the approval of `conn` lapses, about: from when it was given.
    fn approval(
        &self,
        c: &Connector,
        conn: &Conn,
        tier: VaultSource,
        person: &str,
    ) -> Option<ApprovalView> {
        let at = Self::consented_path(c, &conn.key)?;
        let given = self
            .vault_at(tier, &conn.level)
            .ok()?
            .get(&key_at(tier, person, &at))?;
        DateTime::parse_from_rfc3339(given.trim()).ok().map(|t| {
            workforce_skills::adapters::bank::model::Approval::of(
                t.with_timezone(&Utc),
                Utc::now().date_naive(),
            )
            .into()
        })
    }

    /// The person says they renewed `conn`'s approval at the service (Desk's
    /// Reconnect), which Desk can't see, so takes their word for: checks it,
    /// and only on a pass keeps that it was approved now. The only thing
    /// that moves it. A service whose approval doesn't lapse is only
    /// checked.
    pub async fn renew(
        &self,
        person: &str,
        id: &str,
        conn: &str,
    ) -> Result<ConnectionView, Refusal> {
        let c = Self::connector(id)?;
        let conn = self.conn(conn)?;
        let Some(at) = Self::consented_path(c, &conn.key) else {
            return self.test_conn(c, &conn, person).await;
        };
        let tier = self
            .held_at(c, &conn.level, &conn.key, person)
            .ok_or_else(|| {
                Refusal::NotFound(format!("That {} connection isn't there any more", c.name))
            })?;
        let found = self
            .check(
                c,
                person,
                &self.kept(c, &conn.level, &conn.key, tier, person),
            )
            .await;
        if let Found::Works { .. } = &found {
            let path = key_at(tier, person, &at);
            let now = Utc::now().to_rfc3339();
            self.vault_at(tier, &conn.level)?
                .apply(
                    &[VaultOp::Set {
                        path: &path,
                        value: &now,
                    }],
                    VaultActor::User(person),
                )
                .map_err(Refusal::Internal)?;
        }
        self.record(c, &conn, person, &found, false).await;
        self.shown(c, &conn, person).await
    }

    /// Checks connection `conn` again, and keeps what it found.
    pub async fn test(
        &self,
        person: &str,
        id: &str,
        conn: &str,
    ) -> Result<ConnectionView, Refusal> {
        let c = Self::connector(id)?;
        let conn = self.conn(conn)?;
        self.test_conn(c, &conn, person).await
    }

    async fn test_conn(
        &self,
        c: &Connector,
        conn: &Conn,
        person: &str,
    ) -> Result<ConnectionView, Refusal> {
        let tier = self
            .held_at(c, &conn.level, &conn.key, person)
            .ok_or_else(|| {
                Refusal::NotFound(format!("That {} connection isn't there any more", c.name))
            })?;
        let (found, note) = match c.sign_in {
            SignIn::App => self.test_app(c, conn, tier, person).await,
            _ => {
                let fields = self.kept(c, &conn.level, &conn.key, tier, person);
                (self.check(c, person, &fields).await, None)
            }
        };
        self.record_noted(c, conn, person, &found, false, note)
            .await;
        self.shown(c, conn, person).await
    }

    /// Removes connection `conn`'s keys at the tier it is kept at, and
    /// nothing else.
    pub async fn disconnect(
        &self,
        person: &str,
        id: &str,
        conn: &str,
    ) -> Result<Disconnected, Refusal> {
        let c = Self::connector(id)?;
        let conn = self.conn(conn)?;
        self.disconnect_conn(c, &conn, person).await
    }

    async fn disconnect_conn(
        &self,
        c: &Connector,
        conn: &Conn,
        person: &str,
    ) -> Result<Disconnected, Refusal> {
        let tier = self
            .held_at(c, &conn.level, &conn.key, person)
            .ok_or_else(|| {
                Refusal::NotFound(format!("That {} connection isn't there any more", c.name))
            })?;
        let vault = self.vault_at(tier, &conn.level)?;
        let keys: Vec<String> = c
            .held_at(&conn.key)
            .iter()
            .map(|p| key_at(tier, person, p))
            .collect();
        let ops: Vec<VaultOp<'_>> = keys.iter().map(|path| VaultOp::Delete { path }).collect();
        vault
            .apply(&ops, VaultActor::User(person))
            .map_err(Refusal::Internal)?;
        if let Err(e) = self
            .statuses
            .forget(c.id, &conn.level, &conn.key, person)
            .await
        {
            warn!(connector = c.id, error = %format!("{e:#}"), "a connection's checks weren't forgotten");
        }
        self.reload(c, &conn.level, person).await;
        // A key kept at another tier of this level still connects it: an
        // older one saved for the whole hub, say.
        let still = self.held_at(c, &conn.level, &conn.key, person);
        if still.is_none() {
            self.unpick(c, &conn.key, person).await;
        }
        Ok(Disconnected {
            still_kept: still,
            note: still.map(|tier| match tier {
                VaultSource::Global => format!(
                    "A {} key saved for everyone on this hub is still kept, so agents still reach {}. Remove it from the hub's vault to disconnect it fully.",
                    c.name, c.name
                ),
                _ => format!("Another {} key is still kept, so agents still reach {}.", c.name, c.name),
            }),
        })
    }

    /// The one connection of `c` at `level`, for a request from a Desk that
    /// names a level only: its `default`, else its only one.
    async fn only(
        &self,
        person: &str,
        c: &Connector,
        level: &str,
        agent: Option<&str>,
    ) -> Result<Conn, Refusal> {
        let level = self.level(level, agent)?;
        self.migrate(person).await;
        let held = self.instances(c, &level, person);
        let key = match held.iter().find(|(k, _)| k == DEFAULT_INSTANCE) {
            Some((k, _)) => k.clone(),
            None if held.len() == 1 => held[0].0.clone(),
            None if held.is_empty() => {
                return Err(Refusal::NotFound(format!(
                    "{} isn't connected there",
                    c.name
                )))
            }
            None => {
                return Err(Refusal::Invalid(format!(
                    "{} has several connections there: update Desk to choose one",
                    c.name
                )))
            }
        };
        Ok(Conn { level, key })
    }

    /// [`Self::test`] of the level's one connection.
    pub async fn test_level(
        &self,
        person: &str,
        id: &str,
        level: &str,
        agent: Option<&str>,
    ) -> Result<ConnectionView, Refusal> {
        let c = Self::connector(id)?;
        let conn = self.only(person, c, level, agent).await?;
        self.test_conn(c, &conn, person).await
    }

    /// [`Self::disconnect`] of the level's one connection.
    pub async fn disconnect_level(
        &self,
        person: &str,
        id: &str,
        level: &str,
        agent: Option<&str>,
    ) -> Result<Disconnected, Refusal> {
        let c = Self::connector(id)?;
        let conn = self.only(person, c, level, agent).await?;
        self.disconnect_conn(c, &conn, person).await
    }

    /// Loads `c` again on the agents a change at `level` reaches, and for
    /// a service whose approval lapses, moves its watch to the agents that
    /// now watch it.
    async fn reload(&self, c: &Connector, level: &Level, person: &str) {
        let agents: Vec<String> = match level {
            Level::All => self.agents.all().into_iter().map(|a| a.id).collect(),
            Level::Agent(agent) => vec![agent.clone()],
        };
        for agent in agents {
            self.agents.reload(&agent, c.id, person).await;
        }
        if c.renewal.is_some() {
            self.rewatch(person).await;
        }
    }

    /// Moves `person`'s watch of every approval that lapses to the agents
    /// that watch it now: after an agent is removed, its watch is another's.
    pub async fn rewatch(&self, person: &str) {
        for c in catalog::CATALOG.iter().filter(|c| c.renewal.is_some()) {
            let watching = self.watchers(c, person);
            for agent in self.agents.all() {
                self.agents
                    .watch_approvals(&agent.id, person, watching.contains(&agent.id))
                    .await;
            }
        }
    }

    /// The agents that watch `person`'s connections of `c`: for each, the
    /// agent whose own it is, or for one of all their agents' the first by
    /// name it reaches, unless an agent already watching reaches it.
    fn watchers(&self, c: &Connector, person: &str) -> BTreeSet<String> {
        let agents = self.agents.all();
        let mut held: Vec<(Level, String)> = self
            .levels()
            .into_iter()
            .flat_map(|level| {
                self.instances(c, &level, person)
                    .into_iter()
                    .map(move |(key, _)| (level.clone(), key))
            })
            .collect();
        held.sort_by_key(|(level, _)| matches!(level, Level::All));
        held.iter()
            .fold(BTreeSet::new(), |mut watching, (level, key)| {
                let reaching: Vec<&Agent> = agents
                    .iter()
                    .filter(|a| a.binding.reaches(&a.id, c.id, level, key))
                    .collect();
                if !reaching.iter().any(|a| watching.contains(&a.id)) {
                    if let Some(first) = reaching.first() {
                        watching.insert(first.id.clone());
                    }
                }
                watching
            })
    }

    fn insert_flow(
        &self,
        person: &str,
        connector: &'static str,
        level: Level,
        key: Option<String>,
        name: Option<String>,
        again: bool,
    ) -> String {
        let id = Uuid::new_v4().simple().to_string();
        let now = Instant::now();
        let mut flows = self.flows.lock().expect("connection flows poisoned");
        flows.retain(|_, f| f.until > now);
        flows.insert(
            id.clone(),
            Flow {
                person: person.to_string(),
                connector,
                level,
                key,
                name,
                again,
                state: FlowState::Waiting,
                until: now + FLOW_TTL,
            },
        );
        id
    }

    /// [`Self::settle`], with the connection it ended on: an account
    /// connected already is signed in again where it is kept.
    fn settle_at(&self, flow: &str, conn: Conn, state: FlowState) {
        if let Some(f) = self
            .flows
            .lock()
            .expect("connection flows poisoned")
            .get_mut(flow)
            .filter(|f| f.state == FlowState::Waiting)
        {
            f.level = conn.level;
            f.key = Some(conn.key);
        }
        self.settle(flow, state);
    }

    /// The connection sign-in `flow` keeps, as far as it knows it.
    fn flow_conn(&self, flow: &str) -> Option<Conn> {
        let flows = self.flows.lock().expect("connection flows poisoned");
        let f = flows.get(flow)?;
        Some(Conn {
            level: f.level.clone(),
            key: f.key.clone()?,
        })
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

    /// Sign-in `flow`, while it is still waited on.
    fn waiting_flow(&self, flow: &str) -> Option<WaitingFlow> {
        let flows = self.flows.lock().expect("connection flows poisoned");
        let f = flows
            .get(flow)
            .filter(|f| f.state == FlowState::Waiting && f.until > Instant::now())?;
        Some(WaitingFlow {
            person: f.person.clone(),
            level: f.level.clone(),
            key: f.key.clone(),
            name: f.name.clone(),
            again: f.again,
        })
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

    /// Starts a sign-in for a new connection of `id`, named `name`, at
    /// `level`.
    pub async fn start(
        self: &Arc<Self>,
        person: &str,
        id: &str,
        name: Option<&str>,
        level: &str,
        agent: Option<&str>,
    ) -> Result<Started, Refusal> {
        let c = Self::connector(id)?;
        let level = self.level(level, agent)?;
        let name = Self::name(c, name)?;
        self.vault_at(Self::tiers(c, &level)[0], &level)?;
        self.migrate(person).await;
        // A first connection is the adapters' `default`, as a flat one was
        // (for GitHub, the token git reaches any owner with). A later GitHub
        // one is the person's name for it, else its account's login once it
        // is signed in.
        // One signed in to on an app is named after its account too.
        let named_later = c.id == "github" || c.sign_in == SignIn::App;
        let key = match (named_later, &name, self.keys_in_use(c, person).is_empty()) {
            (_, _, true) => Some(DEFAULT_INSTANCE.to_string()),
            (true, None, _) => None,
            (_, Some(name), _) => Some(self.new_key(c, person, name)),
            (false, None, false) => Some(self.new_key(c, person, c.name)),
        };
        match c.sign_in {
            SignIn::Github => self.start_github(person, c, level, key, name).await,
            SignIn::App => self.begin_app(person, c, level, key, name, false),
            SignIn::Fields => Err(Refusal::Invalid(format!(
                "{} is connected with a key",
                c.name
            ))),
        }
    }

    /// Starts GitHub's device sign-in for instance `key` of a setup an
    /// agent runs.
    pub(super) async fn begin_github(
        self: &Arc<Self>,
        person: &str,
        c: &'static Connector,
        level: Level,
        key: Option<String>,
        name: Option<String>,
    ) -> Result<Started, Refusal> {
        self.vault_at(Self::tiers(c, &level)[0], &level)?;
        self.start_github(person, c, level, key, name).await
    }

    async fn start_github(
        self: &Arc<Self>,
        person: &str,
        c: &'static Connector,
        level: Level,
        key: Option<String>,
        name: Option<String>,
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
        // The flow holds its key from now, before GitHub is asked: a second
        // sign-in started meanwhile takes another.
        let flow = self.insert_flow(person, c.id, level.clone(), key, name, false);
        let asked = async {
            let response = self
                .services
                .http
                .post(format!("{}/login/device/code", self.services.github_web))
                .header("Accept", "application/json")
                .form(&[
                    ("client_id", client_id.as_str()),
                    ("scope", GITHUB_DEVICE_SCOPES),
                ])
                .send()
                .await
                .map_err(|_| Refusal::Unavailable("GitHub couldn't be reached".into()))?;
            response
                .json::<DeviceCode>()
                .await
                .map_err(|_| Refusal::Unavailable("GitHub didn't start the sign-in".into()))
        }
        .await;
        let code = match asked {
            Ok(code) => code,
            Err(refused) => {
                self.settle(
                    &flow,
                    FlowState::Failed {
                        error: "GitHub didn't start the sign-in".into(),
                    },
                );
                return Err(refused);
            }
        };
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
                    Ok(Found::Works { account }) => {
                        let (key, name) = self.flow_names(flow);
                        let name = name.unwrap_or_else(|| account.clone());
                        let key = key.unwrap_or_else(|| self.new_key(c, person, &account));
                        let conn = Conn {
                            level: level.clone(),
                            key,
                        };
                        match self.keep(c, level, &conn.key, person, &fields, Some(&name)) {
                            Ok(_) => {
                                let found = Found::Works {
                                    account: account.clone(),
                                };
                                self.record(c, &conn, person, &found, true).await;
                                self.reload(c, level, person).await;
                                FlowState::Connected { account }
                            }
                            Err(_) => FlowState::Failed {
                                error: "Desk couldn't keep the sign-in".into(),
                            },
                        }
                    }
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

    /// The instance and name sign-in `flow` keeps.
    fn flow_names(&self, flow: &str) -> (Option<String>, Option<String>) {
        self.flows
            .lock()
            .expect("connection flows poisoned")
            .get(flow)
            .map(|f| (f.key.clone(), f.name.clone()))
            .unwrap_or_default()
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

#[async_trait]
impl Agents for Registry {
    async fn reload(&self, agent: &str, connector: &str, person: &str) {
        let Some(service) = self.0.registry.get(agent) else {
            return;
        };
        reload_service(&service, connector, person).await;
    }

    async fn watch_approvals(&self, agent: &str, person: &str, on: bool) {
        let Some(scheduler) = self.0.registry.get(agent).and_then(|s| s.scheduler.clone()) else {
            return;
        };
        scheduler
            .keep_for(person, approvals::KEY, on.then(approvals::job))
            .await;
    }

    async fn choose(
        &self,
        agent: &str,
        person: &str,
        choice: &picks::Choice,
    ) -> Result<(), String> {
        let Some(service) = self.0.registry.get(agent) else {
            return Err(format!("There's no agent {agent}"));
        };
        choose_for(&service, person, choice).await
    }

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
                binding: Binding::read(&s.workspace),
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
            binding: Binding::read(&s.workspace),
        })
    }
}

/// Makes `service` use the connections `choice` names, through the hub's
/// one write of its `config.toml` (recorded, admitted, hot-applied; the
/// connector loads again as its binding changes). Tried twice if it changed
/// meanwhile.
pub async fn choose_for(
    service: &crate::serve::AgentService,
    person: &str,
    choice: &picks::Choice,
) -> Result<(), String> {
    let path = service
        .workspace
        .join(workforce_common::config_files::WORKSPACE_CONFIG);
    let store = crate::serve::shared_workflow_store(&service.config).await;
    for _ in 0..2 {
        let read = workforce_common::config_files::read(&path)
            .map_err(|e| format!("Desk couldn't read the agent's settings: {e}"))?;
        let mut doc: toml_edit::DocumentMut = read
            .as_deref()
            .unwrap_or_default()
            .parse()
            .map_err(|_| {
                "The agent's settings file has a mistake in it, so Desk won't change it. Fix it in Settings → This computer.".to_string()
            })?;
        choice.apply(&mut doc)?;
        let body = doc.to_string();
        match crate::agent_config_write::save(
            service,
            service.agent_id.as_str(),
            store.as_deref(),
            &body,
            workforce_orchestrator::workflow_store::EditContext::new("desk", Some(person)),
            |now| now == read.as_deref(),
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(crate::agent_config_write::Refused::Stale { .. }) => continue,
            Err(e) => return Err(format!("The agent's connections weren't changed: {e}")),
        }
    }
    Err("The agent's settings kept changing. Try again.".into())
}

/// Loads `connector` again on `service`, as `person`: Slack reconnects
/// with what is kept now; any other connector that is loaded is unloaded
/// and loaded from the vault again, so one whose keys are gone stays
/// unloaded.
pub async fn reload_service(service: &crate::serve::AgentService, connector: &str, person: &str) {
    if connector == "slack" {
        if let Some(reload) = service.tools.get("system.reload_slack") {
            let ran = workforce_agent::runner::CURRENT_USER_ID
                .scope(person.to_string(), reload.execute(json!({})))
                .await;
            if let Err(e) = ran {
                warn!(error = %format!("{e:#}"), "Slack didn't reconnect after a connection changed");
            }
        }
        return;
    }
    if let Some(vault) = service.vault.clone() {
        reload_skill(
            connector,
            vault,
            service.global_vault.clone(),
            &service.config,
            &service.tools,
            &service.skill_state,
            person,
        );
    }
}

/// [`reload_service`] of one loaded skill.
pub fn reload_skill(
    connector: &str,
    vault: Arc<Vault>,
    global: Option<Arc<Vault>>,
    config: &workforce_common::settings::WorkforceConfig,
    tools: &workforce_agent::tools::registry::ToolRegistry,
    state: &crate::skill_state::SkillStateTracker,
    person: &str,
) {
    let loaded =
        state.is_loaded(connector) || crate::skill_loader::skill_tools_registered(connector, tools);
    if !loaded {
        return;
    }
    crate::skill_loader::unload_skill(connector, tools);
    state.mark_unloaded(connector);
    let reloaded = workforce_agent::runner::CURRENT_USER_ID.sync_scope(person.to_string(), || {
        crate::skill_loader::load_and_track(connector, vault, global, config, tools, state, "vault")
    });
    if let Err(e) = reloaded {
        tracing::info!(connector, error = %format!("{e:#}"), "a connector stays unloaded after its connection changed");
    }
}

/// Once an agent is removed, the approvals it watched for the people who
/// hold this hub are watched by another of their agents.
pub async fn agent_removed(state: &GatewayState) {
    let Some(connections) = INSTALLED.get() else {
        return;
    };
    let Some(identities) = state.identity_store.as_ref() else {
        return;
    };
    let holders = identities
        .list(&workforce_identity::store::IdentityFilter::default())
        .unwrap_or_default()
        .into_iter()
        .filter(|i| crate::authz::is_effective_admin(i.role, i.status));
    for holder in holders {
        connections.rewatch(&holder.id.to_string()).await;
    }
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
            let connections = Connections::new(
                state.vault.clone(),
                Arc::new(Registry(Arc::clone(state))),
                Arc::new(desk),
                Services::live(),
                config.desk.github_client_id.clone(),
                config.gateway.listen.clone(),
            );
            Arc::new(connections)
        })
        .clone();
    router(Arc::clone(state), Arc::clone(&connections)).merge(setup::install(state, connections))
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

/// A Test the person asked for after renewing the service's approval.
#[derive(Deserialize)]
struct Renewal {
    #[serde(default)]
    renewed: bool,
}

#[derive(Deserialize)]
struct At {
    level: String,
    agent: Option<String>,
}

#[derive(Deserialize)]
struct ConnectBody {
    #[serde(default)]
    name: Option<String>,
    level: String,
    agent: Option<String>,
    #[serde(default)]
    fields: BTreeMap<String, String>,
    /// The paired hub these keys were copied from, as Desk's native side
    /// names it (`desk_key_copies` on that hub): said in the audit row.
    #[serde(default)]
    copied_from: Option<String>,
}

#[derive(Deserialize)]
struct StartBody {
    #[serde(default)]
    name: Option<String>,
    level: String,
    agent: Option<String>,
}

#[derive(Deserialize)]
struct FieldsBody {
    #[serde(default)]
    fields: BTreeMap<String, String>,
    /// As [`ConnectBody::copied_from`].
    #[serde(default)]
    copied_from: Option<String>,
}

/// A hub's name as an audit row may say it: one line, at most 80
/// characters; none when empty.
fn hub_name(name: Option<&str>) -> Option<String> {
    let name: String = name?
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// What became of keys Desk copied in from a paired hub.
enum CopyEnd<'a> {
    /// Its check passed, and it is kept as this connection.
    Kept(&'a ConnectionView),
    /// Not kept: refused so, over connection `connection` when it named one.
    NotKept {
        connection: Option<&'a str>,
        refusal: &'a Refusal,
    },
}

/// The audit row for keys of `connector` copied from hub `from`, saying
/// whether their check passed: "Desk copied your Jira key (Acme) from
/// Patchwork: its check passed", or "Desk didn't keep your Jira key from
/// Patchwork: its check failed". The paired hub's own row says only that
/// it handed them over. Never a value.
fn audit_copy(routes: &Routes, person: &str, connector: &str, end: CopyEnd<'_>, from: &str) {
    use workforce_common::admin::AuditOutcome;
    let service = catalog::find(connector).map_or(connector, |c| c.name);
    let (said, connection, check, outcome) = match end {
        CopyEnd::Kept(c) => {
            let of = if c.name == service {
                String::new()
            } else {
                format!(" ({})", c.name)
            };
            (
                format!("Desk copied your {service} key{of} from {from}: its check passed"),
                Some(c.id.as_str()),
                "passed",
                AuditOutcome::Success,
            )
        }
        CopyEnd::NotKept {
            connection,
            refusal,
        } => {
            let (why, check) = match refusal {
                Refusal::TestFailed { .. } => (": its check failed", "failed"),
                _ => ("", "not_run"),
            };
            (
                format!("Desk didn't keep your {service} key from {from}{why}"),
                connection,
                check,
                AuditOutcome::Error,
            )
        }
    };
    info!(target: "sentinel.audit", identity_id = %person, connector, check, "desk_connections.copy: {said}");
    let Some(telemetry) = routes.state.telemetry_store.as_ref() else {
        return;
    };
    let detail = json!({
        "summary": said,
        "identity_id": person,
        "connector": connector,
        "connection": connection,
        "from": from,
        "check": check,
    })
    .to_string();
    if let Err(e) = telemetry.record_audit(
        &format!("user:{person}"),
        "desk_connections.copy",
        &format!(
            "desk_connection:{connector}/{}",
            connection.unwrap_or("new")
        ),
        Some(&detail),
        outcome,
    ) {
        warn!(error = %e, "a copied connection's audit row wasn't written");
    }
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

#[derive(Deserialize)]
struct AppBody {
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

// After `person!`, which its routes use.
pub mod picks;
pub mod setup;

/// Connections' routes over `connections`.
pub fn router(state: Arc<GatewayState>, connections: Arc<Connections>) -> Router {
    Router::new()
        .route("/api/desk/connections", get(list))
        .route(
            "/api/desk/connections/{connector}",
            post(connect).delete(disconnect_level),
        )
        .route("/api/desk/connections/{connector}/test", post(test_level))
        .route("/api/desk/connections/{connector}/app", post(keep_app))
        .route("/api/desk/connections/{connector}/oauth/start", post(start))
        .route(
            "/api/desk/connections/{connector}/{connection}",
            post(reconnect).patch(rename).delete(disconnect),
        )
        .route(
            "/api/desk/connections/{connector}/{connection}/test",
            post(test),
        )
        .route(
            "/api/desk/connections/{connector}/{connection}/approve",
            post(approve_again),
        )
        .route(
            "/api/desk/connection-flows/{flow}",
            get(flow).delete(cancel),
        )
        .route(
            "/api/desk/agents/{agent}/connections",
            get(picks::get_picks).put(picks::put_picks),
        )
        .route("/api/desk/agents/{agent}/documents", get(documents))
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
    let kept = r
        .connections
        .connect(
            &person,
            &connector,
            body.name.as_deref(),
            &body.level,
            body.agent.as_deref(),
            body.fields,
        )
        .await;
    if let Some(from) = hub_name(body.copied_from.as_deref()) {
        let end = match &kept {
            Ok(c) => CopyEnd::Kept(c),
            Err(refusal) => CopyEnd::NotKept {
                connection: None,
                refusal,
            },
        };
        audit_copy(&r, &person, &connector, end, &from);
    }
    kept.map(|c| Json(json!({ "connection": c })))
        .into_response()
}

async fn reconnect(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path((connector, connection)): Path<(String, String)>,
    Json(body): Json<FieldsBody>,
) -> Response {
    let person = person!(r, h, q);
    let kept = r
        .connections
        .reconnect(&person, &connector, &connection, body.fields)
        .await;
    if let Some(from) = hub_name(body.copied_from.as_deref()) {
        let end = match &kept {
            Ok(c) => CopyEnd::Kept(c),
            Err(refusal) => CopyEnd::NotKept {
                connection: Some(&connection),
                refusal,
            },
        };
        audit_copy(&r, &person, &connector, end, &from);
    }
    kept.map(|c| Json(json!({ "connection": c })))
        .into_response()
}

async fn rename(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path((connector, connection)): Path<(String, String)>,
    Json(body): Json<NameBody>,
) -> Response {
    let person = person!(r, h, q);
    r.connections
        .rename(&person, &connector, &connection, &body.name)
        .await
        .map(|c| Json(json!({ "connection": c })))
        .into_response()
}

async fn test(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Query(renewal): Query<Renewal>,
    Path((connector, connection)): Path<(String, String)>,
) -> Response {
    let person = person!(r, h, q);
    let tested = match renewal.renewed {
        true => r.connections.renew(&person, &connector, &connection).await,
        false => r.connections.test(&person, &connector, &connection).await,
    };
    tested
        .map(|c| Json(json!({ "connection": c })))
        .into_response()
}

async fn disconnect(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path((connector, connection)): Path<(String, String)>,
) -> Response {
    let person = person!(r, h, q);
    r.connections
        .disconnect(&person, &connector, &connection)
        .await
        .map(Json)
        .into_response()
}

async fn test_level(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(connector): Path<String>,
    Json(at): Json<At>,
) -> Response {
    let person = person!(r, h, q);
    r.connections
        .test_level(&person, &connector, &at.level, at.agent.as_deref())
        .await
        .map(|c| Json(json!({ "connection": c })))
        .into_response()
}

async fn disconnect_level(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(connector): Path<String>,
    Query(at): Query<At>,
) -> Response {
    let person = person!(r, h, q);
    r.connections
        .disconnect_level(&person, &connector, &at.level, at.agent.as_deref())
        .await
        .map(Json)
        .into_response()
}

async fn keep_app(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(connector): Path<String>,
    Json(body): Json<AppBody>,
) -> Response {
    let person = person!(r, h, q);
    r.connections
        .keep_app(&person, &connector, &body.client_id, &body.client_secret)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

async fn approve_again(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path((connector, connection)): Path<(String, String)>,
) -> Response {
    let person = person!(r, h, q);
    r.connections
        .approve_again(&person, &connector, &connection)
        .await
        .map(Json)
        .into_response()
}

async fn start(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(connector): Path<String>,
    Json(at): Json<StartBody>,
) -> Response {
    let person = person!(r, h, q);
    r.connections
        .start(
            &person,
            &connector,
            at.name.as_deref(),
            &at.level,
            at.agent.as_deref(),
        )
        .await
        .map(Json)
        .into_response()
}

/// Agent `agent`'s documents folder, `{path, exists, state}`: made when
/// the agent is equipped, never here.
async fn documents(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(agent): Path<String>,
) -> Response {
    let _person = person!(r, h, q);
    // Only an agent the caller may reach: another's is no agent at all.
    let service = match crate::gateway::authorise_agent_reader(
        &r.state,
        &h,
        q.token.as_deref(),
        &agent,
    )
    .await
    {
        Ok((service, _)) => service,
        Err(refused) => return refused.into_response(),
    };
    let path = service.workspace.join("documents");
    // `made`, `missing` (made when the agent is next equipped), or
    // `not_a_folder`: a link or a file, which Desk never uses.
    let state = match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_dir() => "made",
        Ok(_) => "not_a_folder",
        Err(_) => "missing",
    };
    Json(json!({ "path": path, "exists": state == "made", "state": state })).into_response()
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

/// What GitHub's device sign-in asks for: repositories and their issues,
/// the person's organisations, and Projects, which a Desk board synced with
/// a GitHub project reads and arranges (`project` reads and writes them).
pub const GITHUB_DEVICE_SCOPES: &str = "repo read:org project";

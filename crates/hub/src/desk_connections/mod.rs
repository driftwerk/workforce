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
//!   device sign-in, or Google's on the person's own OAuth client, for a new
//!   connection; `POST /api/desk/connections/google/client` keeps that
//!   client.
//! - `GET|DELETE /api/desk/connection-flows/{flow}`: a sign-in's progress,
//!   or cancelling it.
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

pub mod catalog;
pub mod check;
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
/// and which use its own connection. AC9a's grammar, read raw here so the
/// listing follows it on any hub version.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Binding {
    /// `None`: every connector, as before the key existed.
    pub enabled: Option<Vec<String>>,
    pub own: Vec<String>,
}

impl Binding {
    /// `[skills]` of the `config.toml` in `workspace`.
    pub fn read(workspace: &std::path::Path) -> Self {
        let body = std::fs::read_to_string(
            workspace.join(workforce_common::config_files::WORKSPACE_CONFIG),
        )
        .unwrap_or_default();
        let Ok(doc) = body.parse::<toml::Table>() else {
            return Self::default();
        };
        let skills = doc.get("skills").and_then(toml::Value::as_table);
        let names = |key: &str| {
            skills
                .and_then(|t| t.get(key))
                .and_then(toml::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
        };
        Self {
            enabled: names("enabled"),
            own: names("own_credential").unwrap_or_default(),
        }
    }

    fn is_on(&self, connector: &str) -> bool {
        self.enabled
            .as_ref()
            .is_none_or(|e| e.iter().any(|c| c == connector))
    }

    /// Whether the agent `agent` with this binding reaches `connector`'s
    /// connections kept at `level`, by the tiers its pin admits: none when
    /// the connector is off; only its own when it uses its own connection;
    /// only the person's when it names `enabled` and uses theirs; both when
    /// it names neither, as before the keys existed.
    fn reaches(&self, agent: &str, connector: &str, level: &Level) -> bool {
        if !self.is_on(connector) {
            return false;
        }
        let own = self.own.iter().any(|o| o == connector);
        match level {
            Level::Agent(a) if a != agent => false,
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
    /// The vault tier it is kept at.
    pub source: VaultSource,
    /// The agents it reaches.
    pub used_by: Vec<UsedBy>,
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
    /// Whether it holds more than one connection (Slack holds one).
    pub several: bool,
    /// What a new connection's name should be, as the form says.
    pub name_hint: &'static str,
    pub connections: Vec<ConnectionView>,
    /// Every agent it reaches, through any of its connections.
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
    level: Level,
    /// The instance it keeps; GitHub's is its account's login when the
    /// person named none.
    key: Option<String>,
    /// The name the person gave it, if any.
    name: Option<String>,
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
    /// `[github] default_owner`: the owner a flat GitHub connection reached
    /// by default, which its instance is named after when it moves.
    pub github_owner: Option<String>,
    flows: Mutex<HashMap<String, Flow>>,
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
        google_redirect: String,
    ) -> Self {
        Self {
            global,
            agents,
            statuses,
            services,
            github_client_id: github_client_id.filter(|id| !id.trim().is_empty()),
            google_redirect,
            github_owner: None,
            flows: Mutex::new(HashMap::new()),
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
        if key.is_empty() || key.len() > 64 || key.contains('/') {
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
    fn keys_in_use(&self, c: &Connector, person: &str) -> BTreeSet<String> {
        self.levels()
            .iter()
            .flat_map(|level| self.instances(c, level, person))
            .map(|(key, _)| key)
            .collect()
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

    /// The kept fields of instance `key` at `tier`.
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
            .filter_map(|f| {
                vault
                    .get(&key_at(tier, person, &c.field_at(f, key)))
                    .map(|v| (f.name.to_string(), v))
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
            .filter(|a| a.binding.reaches(&a.id, c.id, &conn.level))
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
            source: tier,
            used_by,
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
                    match self.migrate_at(c, &level, tier, &vault, person, checked) {
                        Ok(Some(key)) => {
                            moved = true;
                            if key != DEFAULT_INSTANCE {
                                if let Err(e) = self
                                    .statuses
                                    .moved(c.id, &level, person, DEFAULT_INSTANCE, &key)
                                    .await
                                {
                                    warn!(connector = c.id, error = %format!("{e:#}"), "a moved connection's check stayed behind");
                                }
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

    /// [`Self::migrate`] of `c` at one tier: the instance it moved to or
    /// named, if anything changed.
    ///
    /// It becomes the adapters' `default` (GitHub's: the owner it reached),
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
        let mut key = match c.id {
            "github" => self
                .github_owner
                .as_deref()
                .or(account.as_deref())
                .map(slug)
                .filter(|k| !k.is_empty())
                .unwrap_or_else(|| DEFAULT_INSTANCE.to_string()),
            _ => DEFAULT_INSTANCE.to_string(),
        };
        // The person's connection for all their agents keeps its key.
        let taken = *level != Level::All
            && self
                .levels()
                .iter()
                .filter(|l| *l != level)
                .any(|l| self.instances(c, l, person).iter().any(|(k, _)| *k == key));
        if taken {
            key = self.new_key(c, person, &name);
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
                    several: c.named(),
                    name_hint: c.name_hint,
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
        let (status, account, detail) = match found {
            Found::Works { account } => (Status::Connected, Some(account.clone()), None),
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
    async fn check(&self, c: &Connector, fields: &BTreeMap<String, String>) -> Found {
        match self.services.check(c.id, fields).await {
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
        let account = match self.check(c, &fields).await {
            Found::Works { account } => account,
            other => return Err(Self::failed(other)),
        };
        let site = c.site.and_then(|s| fields.get(s)).map(String::as_str);
        let name = name.unwrap_or_else(|| name_of(c, Some(&account), site));
        let key = match (c.id, self.keys_in_use(c, person).is_empty()) {
            // GitHub's instance is the owner it reaches by default.
            ("github", _) => self.new_key(c, person, &name),
            // A first connection is the adapters' `default`, as a flat one
            // was.
            (_, true) => DEFAULT_INSTANCE.to_string(),
            _ => self.new_key(c, person, &name),
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
        let found = self.check(c, &fields).await;
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
        let found = match c.sign_in {
            SignIn::Google => self.test_google(conn, tier, person).await,
            _ => {
                let fields = self.kept(c, &conn.level, &conn.key, tier, person);
                self.check(c, &fields).await
            }
        };
        self.record(c, conn, person, &found, false).await;
        self.shown(c, conn, person).await
    }

    async fn test_google(&self, conn: &Conn, tier: VaultSource, person: &str) -> Found {
        let Some((id, secret, _)) = self.google_client() else {
            return Found::Refused("Your Google OAuth client isn't saved in Desk any more".into());
        };
        let Ok(vault) = self.vault_at(tier, &conn.level) else {
            return Found::Failed("Desk couldn't read this connection".into());
        };
        let google = catalog::find("google").expect("google is in the catalog");
        let Some(refresh) = vault.get(&key_at(tier, person, &google.primary_at(&conn.key))) else {
            return Found::Refused("Google isn't signed in".into());
        };
        let email = vault.get(&key_at(tier, person, &google.at(&conn.key, "email")));
        let mut fields = BTreeMap::new();
        fields.insert("refresh".into(), refresh.clone());
        fields.insert("secret".into(), secret.clone());
        match self.services.google(&id, &secret, &refresh, email).await {
            Found::Refused(e) => Found::Refused(scrub(&e, &fields)),
            Found::Failed(e) => Found::Failed(scrub(&e, &fields)),
            works => works,
        }
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

    /// Loads `c` again on the agents a change at `level` reaches.
    async fn reload(&self, c: &Connector, level: &Level, person: &str) {
        let agents: Vec<String> = match level {
            Level::All => self.agents.all().into_iter().map(|a| a.id).collect(),
            Level::Agent(agent) => vec![agent.clone()],
        };
        for agent in agents {
            self.agents.reload(&agent, c.id, person).await;
        }
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

    fn insert_flow(
        &self,
        person: &str,
        level: Level,
        key: Option<String>,
        name: Option<String>,
    ) -> String {
        let id = Uuid::new_v4().simple().to_string();
        let now = Instant::now();
        let mut flows = self.flows.lock().expect("connection flows poisoned");
        flows.retain(|_, f| f.until > now);
        flows.insert(
            id.clone(),
            Flow {
                person: person.to_string(),
                level,
                key,
                name,
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
        // GitHub's instance is its owner: the person's name for it, else
        // its account's login once it is signed in.
        let key = match (c.id, &name, self.keys_in_use(c, person).is_empty()) {
            ("github", None, _) => None,
            ("github", Some(name), _) => Some(self.new_key(c, person, name)),
            // A first connection is the adapters' `default`, as a flat one
            // was.
            (_, _, true) => Some(DEFAULT_INSTANCE.to_string()),
            (_, Some(name), false) => Some(self.new_key(c, person, name)),
            (_, None, false) => Some(self.new_key(c, person, c.name)),
        };
        match c.sign_in {
            SignIn::Github => self.start_github(person, c, level, key, name).await,
            SignIn::Google => self.start_google(
                person,
                level,
                key.unwrap_or_else(|| DEFAULT_INSTANCE.to_string()),
                name,
            ),
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
        let flow = self.insert_flow(person, level.clone(), key, name);
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

    fn start_google(
        &self,
        person: &str,
        level: Level,
        key: String,
        name: Option<String>,
    ) -> Result<Started, Refusal> {
        use workforce_skills::adapters::google::oauth::{start_web_flow, OAuthClientCreds};
        use workforce_skills::adapters::google::web_flow::DeskFlow;
        let Some((client_id, client_secret, redirect)) = self.google_client() else {
            return Err(Refusal::Invalid(
                "Add your Google OAuth client first: its client ID and secret".into(),
            ));
        };
        let redirect_uri = redirect.unwrap_or_else(|| self.google_redirect.clone());
        if !check::is_loopback_url(&redirect_uri) {
            return Err(Refusal::Invalid(format!(
                "Desk signs in to Google on this computer only. Set your OAuth client's redirect to {} (a Desktop app client allows it).",
                self.google_redirect
            )));
        }
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
        let flow = self.insert_flow(person, level, Some(key.clone()), name);
        crate::google_oauth_callback::google_web_flow_store().insert_flow(
            web.state,
            person.to_string(),
            key,
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

    /// Google signed `person` in for `flow`: keeps the tokens as the flow's
    /// instance at its tier, unless it was cancelled meanwhile.
    pub async fn google_signed_in(
        &self,
        flow: &str,
        person: &str,
        tokens: &workforce_skills::adapters::google::oauth::StoredTokens,
    ) -> Result<String, String> {
        let (level, key, name) = {
            let flows = self.flows.lock().expect("connection flows poisoned");
            match flows.get(flow) {
                Some(f) if f.person == person && f.state == FlowState::Waiting => (
                    f.level.clone(),
                    f.key
                        .clone()
                        .unwrap_or_else(|| DEFAULT_INSTANCE.to_string()),
                    f.name.clone(),
                ),
                _ => {
                    return Err(
                        "This sign-in was cancelled or has expired. Nothing was saved.".into(),
                    )
                }
            }
        };
        let c = catalog::find("google").expect("google is in the catalog");
        let tier = Self::tiers(c, &level)[0];
        let failed = |this: &Self| {
            this.settle(
                flow,
                FlowState::Failed {
                    error: "Desk couldn't keep the sign-in".into(),
                },
            );
            Err("Desk couldn't keep the sign-in.".to_string())
        };
        let vault = match self.vault_at(tier, &level) {
            Ok(vault) => vault,
            Err(_) => return failed(self),
        };
        let store =
            crate::user_config::UserAwareVault::for_user(Arc::clone(&vault), person.to_string());
        if let Err(e) = workforce_skills::adapters::google::oauth::persist_tokens_at_user_scope(
            &store, person, &key, tokens,
        ) {
            tracing::error!(error = %format!("{e:#}"), "keeping a Google sign-in failed");
            return failed(self);
        }
        let account = tokens.email.clone().unwrap_or_else(|| "Google".into());
        let label = key_at(tier, person, &c.label_at(&key).expect("google is named"));
        let name = name.unwrap_or_else(|| account.clone());
        if let Err(e) = vault.apply(
            &[VaultOp::Set {
                path: &label,
                value: &name,
            }],
            VaultActor::User(person),
        ) {
            warn!(error = %format!("{e:#}"), "a Google connection's name wasn't kept");
        }
        let found = Found::Works {
            account: account.clone(),
        };
        let conn = Conn { level, key };
        self.record(c, &conn, person, &found, true).await;
        self.reload(c, &conn.level, person).await;
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

#[async_trait]
impl Agents for Registry {
    async fn reload(&self, agent: &str, connector: &str, person: &str) {
        let Some(service) = self.0.registry.get(agent) else {
            return;
        };
        reload_service(&service, connector, person).await;
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
            let mut connections = Connections::new(
                state.vault.clone(),
                Arc::new(Registry(Arc::clone(state))),
                Arc::new(desk),
                Services::live(),
                config.desk.github_client_id.clone(),
                google_redirect(&config.gateway.listen),
            );
            connections.github_owner = config
                .github
                .default_owner
                .clone()
                .filter(|o| !o.trim().is_empty());
            Arc::new(connections)
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
    #[serde(default)]
    name: Option<String>,
    level: String,
    agent: Option<String>,
    #[serde(default)]
    fields: BTreeMap<String, String>,
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
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
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
            post(connect).delete(disconnect_level),
        )
        .route("/api/desk/connections/{connector}/test", post(test_level))
        .route("/api/desk/connections/{connector}/client", post(client))
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
            body.name.as_deref(),
            &body.level,
            body.agent.as_deref(),
            body.fields,
        )
        .await
        .map(|c| Json(json!({ "connection": c })))
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
    r.connections
        .reconnect(&person, &connector, &connection, body.fields)
        .await
        .map(|c| Json(json!({ "connection": c })))
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
    Path((connector, connection)): Path<(String, String)>,
) -> Response {
    let person = person!(r, h, q);
    r.connections
        .test(&person, &connector, &connection)
        .await
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

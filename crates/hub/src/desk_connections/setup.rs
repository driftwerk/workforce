//! An agent sets a connection up itself, on this computer: `connections.*`.
//!
//! Desk runs locally, so an agent can make a service's key the way the
//! person would, in the person's Chrome (through Desk's extension), and
//! keep it where Connections keeps one. The person signs in to the service;
//! the agent does the rest:
//!
//! 1. `connections.setup {service, name?, for?}` starts a setup. The hub
//!    makes it, and from it alone come the vault tier, the agent the
//!    connection is for, and its instance: no later call names any of them.
//! 2. `connections.keep {setup, field, value}` keeps a field that isn't a
//!    secret: a site (`https://<name>.atlassian.net` for Jira, a public
//!    `https` origin for Metabase, a region for Datadog), an email. Once a
//!    key is held, nothing more is kept: the key goes nowhere else.
//! 3. The key itself is read by the browser's `browser.capture_key`, in
//!    the extension, off the setup's key page ([`Setups::key_page`]), and
//!    handed straight to [`Setups::keep_key`]: it never passes through the
//!    model. It is held in memory for the setup, and taken into the
//!    session's secret cache, so should it show up again it reaches the
//!    model as a marker. Once every key is held the connection is tested
//!    with the service, at the address kept for it (Metabase's only at the
//!    origin the key was read on), and kept only on a pass.
//! 4. `connections.test {setup}` tests again, and finishes GitHub's device
//!    sign-in.
//! 5. `connections.use {setup | service + connection, agent?, write?,
//!    default?}` turns a connection on for an agent, through the hub's one
//!    write of its `config.toml`.
//!
//! `connections.list` and `connections.step` (which marks a step of the
//! card: current, waiting on the person, done) touch nothing.
//!
//! **Who may do what.** A setup is the person's (the one whose Desk this
//! is), the agent's that made it and the conversation's: no other agent,
//! person or session can use it, and a call without a conversation is
//! refused. An agent sets up a connection just for itself; one for all the
//! person's agents, or for another agent, and turning one on for another
//! agent, are the person's overseer's alone. Keeping a field, starting
//! GitHub's sign-in, testing and turning on are connection writes to Desk's
//! gate: freely for an unbound overseer, asking first below, Allow once or
//! Deny, its question naming where the key goes and for whom. The person's
//! password is never read or typed: signing in is a step that waits on
//! them.
//!
//! The card the chat shows (`GET /api/desk/connection-setups?agent=`) is
//! the hub's record of the setup, never what the model wrote.

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
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::secret::Secret;
use workforce_core::computer::Effect;
use workforce_desk::connections::Level;
use workforce_skills::adapters::browser::keys as browser_keys;

use super::catalog::{self, Connector};
use super::check::Found;
use super::{Conn, Connections, Refusal};

/// How long a setup is kept.
const SETUP_TTL: Duration = Duration::from_secs(60 * 60);
/// The most setups one person keeps at once.
const MAX_SETUPS: usize = 16;
/// The tools, the ones that change nothing first.
pub const NAMES: [&str; 6] = [
    "connections.list",
    "connections.step",
    "connections.setup",
    "connections.keep",
    "connections.test",
    "connections.use",
];

/// Datadog's regions, as a setup keeps its site.
const DATADOG_SITES: &[&str] = &["us1", "us3", "us5", "eu1", "ap1", "us1-fed"];

/// Where a step of a setup is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Todo,
    Current,
    /// Waiting on the person: signing in, approving a code.
    Waiting,
    Done,
    Failed,
}

/// One step of a setup, as its card shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub id: &'static str,
    pub label: String,
    pub state: StepState,
    pub note: Option<String>,
}

/// How a service is set up in the browser.
struct Recipe {
    /// The page its key is made on.
    page: fn(&BTreeMap<String, String>) -> Option<String>,
    /// The origins a key of it may be read on.
    origins: fn(&BTreeMap<String, String>) -> Vec<String>,
    /// The paths there: each, or one below it, whole segments only.
    paths: &'static [&'static str],
    /// Whether a path may start with one segment of the account's own (a
    /// Linear workspace's) before them.
    workspace_segment: bool,
    /// Whether a value is shaped like `field`'s.
    shaped: fn(&str, &str) -> bool,
    /// What any of its keys looks like, whole, as a pattern: what the
    /// browser checks a read against and hides on its key pages. Looser
    /// than [`Self::shaped`], which is checked per field after.
    shape: &'static str,
    where_label: Option<&'static str>,
    page_label: &'static str,
    sign_in_label: &'static str,
    key_label: &'static str,
}

fn plain_token(value: &str, min: usize) -> bool {
    value.len() >= min
        && value.len() <= 4096
        && value
            .chars()
            .all(|c| c.is_ascii_graphic() && c != '"' && c != '\'')
}

fn datadog_web(fields: &BTreeMap<String, String>) -> String {
    match fields.get("site").map(|s| s.trim()).unwrap_or("us1") {
        "eu1" => "https://app.datadoghq.eu".into(),
        "us3" => "https://us3.datadoghq.com".into(),
        "us5" => "https://us5.datadoghq.com".into(),
        "ap1" => "https://ap1.datadoghq.com".into(),
        "us1-fed" => "https://app.ddog-gov.com".into(),
        _ => "https://app.datadoghq.com".into(),
    }
}

/// `raw` as the site a setup keeps for `service`: an `https` origin and
/// nothing more (no path, query, fragment or credentials), on a public
/// name: never an IP address, `localhost` or a name without a dot. Jira's
/// is a Jira Cloud site, `<name>.atlassian.net`, on its default port.
fn site_origin(service: &str, raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    let given = match raw.contains("://") {
        true => raw.to_string(),
        false => format!("https://{raw}"),
    };
    let alone = "Give the site's address alone, like https://acme.atlassian.net".to_string();
    let u = url::Url::parse(&given).map_err(|_| alone.clone())?;
    if u.scheme() != "https" {
        return Err("Desk sends a key only over https: give the site's https address.".into());
    }
    if !u.username().is_empty()
        || u.password().is_some()
        || !matches!(u.path(), "" | "/")
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(alone);
    }
    let host = match u.host() {
        Some(url::Host::Domain(d)) => d.trim_end_matches('.').to_ascii_lowercase(),
        _ => return Err("Give the site by its name, not an IP address.".into()),
    };
    if host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || !host.contains('.')
    {
        return Err("Desk sets up sites on the internet only. Ask the person to connect this one in Settings → Connections.".into());
    }
    if service == "jira" && (!host.ends_with(".atlassian.net") || u.port().is_some()) {
        return Err("Desk sets up Jira Cloud sites only (<name>.atlassian.net). Ask the person to connect another in Settings → Connections.".into());
    }
    Ok(match u.port() {
        Some(port) => format!("https://{host}:{port}"),
        None => format!("https://{host}"),
    })
}

/// `connector`'s key page at `origin`, as the browser's registry holds it.
fn browser_page(connector: &str, origin: &str) -> Option<browser_keys::KeyPage> {
    let r = recipe(connector)?;
    Some(browser_keys::KeyPage {
        connector: connector.to_string(),
        origin: origin.to_string(),
        paths: r.paths.iter().map(|p| p.to_string()).collect(),
        workspace_segment: r.workspace_segment,
        shape: regex::Regex::new(r.shape).expect("a recipe's shape compiles"),
    })
}

/// Registers every key page, so their text is hidden and they are never
/// pictured, whoever opens them and whether or not a setup is running:
/// Jira's, Linear's, GitHub's and each Datadog region's at their origins,
/// and Metabase's, whose address is each person's own, on any `https`
/// origin. Nothing is registered later, so the registry never grows.
pub fn register_key_pages() {
    for service in ["jira", "linear", "github"] {
        if let Some(r) = recipe(service) {
            for origin in (r.origins)(&BTreeMap::new()) {
                if let Some(page) = browser_page(service, &origin) {
                    browser_keys::register(page);
                }
            }
        }
    }
    for site in DATADOG_SITES {
        let fields = BTreeMap::from([("site".to_string(), site.to_string())]);
        if let Some(page) = browser_page("datadog", &datadog_web(&fields)) {
            browser_keys::register(page);
        }
    }
    if let Some(page) = browser_page("metabase", "") {
        browser_keys::register_anywhere(page);
    }
}

fn metabase_origin(fields: &BTreeMap<String, String>) -> Option<String> {
    let site = super::check::address(fields.get("site_url")?);
    workforce_skills::adapters::browser::origin_of(&site)
}

fn recipe(service: &str) -> Option<Recipe> {
    Some(match service {
        "jira" => Recipe {
            page: |_| Some("https://id.atlassian.com/manage-profile/security/api-tokens".into()),
            origins: |_| vec!["https://id.atlassian.com".into()],
            paths: &["/manage-profile/security/api-tokens"],
            workspace_segment: false,
            shaped: |_, v| plain_token(v, 24),
            shape: r"[!#-&(-~]{24,4096}",
            where_label: Some("Find your Atlassian site"),
            page_label: "Open id.atlassian.com › Security › API tokens",
            sign_in_label: "Sign in to Atlassian",
            key_label: "Make an API token and save it to the vault",
        },
        "linear" => Recipe {
            page: |_| Some("https://linear.app/settings/account/security".into()),
            origins: |_| vec!["https://linear.app".into()],
            // Linear's settings sit under the workspace's own segment:
            // `/acme/settings/account/security`.
            paths: &["/settings/account/security", "/settings/api"],
            workspace_segment: true,
            shaped: |_, v| v.starts_with("lin_api_") && plain_token(v, 20),
            shape: r"lin_api_[!#-&(-~]{12,4088}",
            where_label: None,
            page_label: "Open Linear › Settings › Security & access",
            sign_in_label: "Sign in to Linear",
            key_label: "Make a personal API key and save it to the vault",
        },
        "datadog" => Recipe {
            page: |f| Some(format!("{}/organization-settings/api-keys", datadog_web(f))),
            origins: |f| vec![datadog_web(f)],
            paths: &[
                "/organization-settings/api-keys",
                "/organization-settings/application-keys",
                "/personal-settings/application-keys",
            ],
            workspace_segment: false,
            shaped: |field, v| {
                let hex = v.chars().all(|c| c.is_ascii_hexdigit());
                match field {
                    "api_key" => hex && v.len() == 32,
                    _ => hex && v.len() == 40,
                }
            },
            shape: r"[0-9a-fA-F]{40}|[0-9a-fA-F]{32}",
            where_label: Some("Find your Datadog site"),
            page_label: "Open Datadog › Organization settings › Keys",
            sign_in_label: "Sign in to Datadog",
            key_label: "Make an API key and an application key and save them to the vault",
        },
        "metabase" => Recipe {
            page: |f| {
                metabase_origin(f).map(|o| format!("{o}/admin/settings/authentication/api-keys"))
            },
            origins: |f| metabase_origin(f).into_iter().collect(),
            paths: &["/admin/settings/authentication/api-keys"],
            workspace_segment: false,
            shaped: |_, v| v.starts_with("mb_") && plain_token(v, 16),
            shape: r"mb_[!#-&(-~]{13,4093}",
            where_label: Some("Find your Metabase address"),
            page_label: "Open Metabase › Admin › Authentication › API keys",
            sign_in_label: "Sign in to Metabase",
            key_label: "Make an API key and save it to the vault",
        },
        "github" => Recipe {
            page: |_| Some("https://github.com/settings/personal-access-tokens/new".into()),
            origins: |_| vec!["https://github.com".into()],
            paths: &["/settings/tokens", "/settings/personal-access-tokens"],
            workspace_segment: false,
            shaped: |_, v| {
                (v.starts_with("github_pat_") || v.starts_with("ghp_")) && plain_token(v, 20)
            },
            shape: r"(?:github_pat_|ghp_)[!#-&(-~]{9,4085}",
            where_label: None,
            page_label: "Open GitHub › Settings › Personal access tokens",
            sign_in_label: "Sign in to GitHub",
            key_label: "Make a token and save it to the vault",
        },
        _ => return None,
    })
}

/// Why a service isn't one an agent sets up, and what the person does.
fn by_hand(service: &str) -> &'static str {
    match service {
        "google" => "Google is connected by the person: it needs their own OAuth client and their consent on Google's page. Ask them to use Settings → Connections → Google → Connect.",
        "email" => "Email is connected by the person: it takes an app password their provider shows only to them. Ask them to use Settings → Connections → Email → Connect.",
        "slack" => "Slack is connected by the person: it takes their Slack app's bot token. Ask them to use Settings → Connections → Slack → Connect.",
        _ => "Desk doesn't set that service up itself. Ask the person to connect it in Settings → Connections.",
    }
}

/// A setup under way.
struct Setup {
    id: String,
    person: String,
    /// The agent running it.
    by: String,
    session: Option<String>,
    service: &'static str,
    name: String,
    key: String,
    level: Level,
    /// The agent it is to be turned on for.
    for_agent: Option<String>,
    steps: Vec<Step>,
    fields: BTreeMap<String, String>,
    secrets: BTreeMap<String, Secret<String>>,
    /// GitHub's device sign-in, where it signs in that way.
    flow: Option<String>,
    /// The origin its key was read on: the only one it is tested at.
    read_at: Option<String>,
    started_at: DateTime<Utc>,
    until: Instant,
    /// The connection it made, once tested.
    connected: Option<Connected>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Connected {
    /// Desk's id of it; never shown.
    pub connection: String,
    pub account: String,
    /// Whom it is on for, once turned on.
    pub on_for: Option<String>,
    pub write: Option<bool>,
}

/// A setup as its card shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupView {
    pub id: String,
    pub agent: String,
    pub session: Option<String>,
    pub service: &'static str,
    pub service_name: &'static str,
    pub name: String,
    pub site: Option<String>,
    pub steps: Vec<Step>,
    pub started_at: DateTime<Utc>,
    pub connected: Option<Connected>,
}

impl Setup {
    fn view(&self) -> SetupView {
        let c = catalog::find(self.service).expect("a setup's service is in the catalog");
        SetupView {
            id: self.id.clone(),
            agent: self.by.clone(),
            session: self.session.clone(),
            service: self.service,
            service_name: c.name,
            name: self.name.clone(),
            site: c.site.and_then(|s| self.fields.get(s).cloned()),
            steps: self.steps.clone(),
            started_at: self.started_at,
            connected: self.connected.clone(),
        }
    }

    fn mark(&mut self, id: &str, state: StepState, note: Option<String>) {
        for step in self.steps.iter_mut() {
            if step.id == id {
                step.state = state;
                if note.is_some() {
                    step.note = note.clone();
                }
            }
        }
        // The first step not done is the one under way.
        if state == StepState::Done {
            if let Some(next) = self.steps.iter_mut().find(|s| s.state == StepState::Todo) {
                next.state = StepState::Current;
            }
        }
    }

    fn relabel(&mut self, id: &str, label: String) {
        if let Some(step) = self.steps.iter_mut().find(|s| s.id == id) {
            step.label = label;
        }
    }
}

/// Who is calling a `connections.*` tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub person: String,
    pub agent: String,
    pub session: Option<String>,
    /// The key secrets it reads are taken into the cache under.
    pub session_key: Option<String>,
    /// It is the person's overseer.
    pub overseer: bool,
}

/// The page a setup's key is read off, for the browser's
/// `browser.capture_key`: the origin and path prefixes it sends the
/// extension's read, and the key fields still to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPage {
    pub origin: String,
    pub paths: Vec<&'static str>,
    /// Whether one segment of the account's own may come first (Linear's
    /// `/acme/settings/account/security`).
    pub workspace_segment: bool,
    pub fields: Vec<&'static str>,
}

impl KeyPage {
    /// Whether `path` is one of this page's: one of [`Self::paths`] or
    /// below it, by whole segments, after the account's own segment where
    /// one may come first.
    pub fn holds(&self, path: &str) -> bool {
        let path = path.trim_end_matches('/');
        let under = |path: &str| {
            self.paths.iter().any(|p| {
                path == *p
                    || path
                        .strip_prefix(p)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        };
        if under(path) {
            return true;
        }
        self.workspace_segment
            && path
                .strip_prefix('/')
                .and_then(|rest| rest.split_once('/'))
                .is_some_and(|(segment, rest)| {
                    !segment.is_empty()
                        && segment
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                        && under(&format!("/{rest}"))
                })
    }
}

/// What a key handed to [`Setups::keep_key`] came to: never any of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeptKey {
    pub kept: bool,
    pub connector: &'static str,
    pub field: &'static str,
    pub length: usize,
    /// `ready`: every key is held, for the agent to check with
    /// `connections.test`; or `still needs <field>`.
    pub test: String,
}

/// What the gate's question says of what the hub couldn't resolve.
const UNKNOWN: &str = "unknown";

/// The hub's facts about callers.
pub trait Callers: Send + Sync {
    /// Whether `person` is the one whose Desk this is.
    fn holds_the_hub(&self, person: &str) -> bool;
    /// Whether `agent` is the person's overseer.
    fn oversees(&self, agent: &str) -> bool;
    /// The agent `name` (its name or id) names, if there is one.
    fn agent_named(&self, name: &str) -> Option<String>;
}

/// The setups under way, over Connections.
pub struct Setups {
    pub connections: Arc<Connections>,
    pub callers: Arc<dyn Callers>,
    setups: Mutex<HashMap<String, Setup>>,
}

/// The longest a field kept by `keep` is.
const MAX_FIELD: usize = 2048;

impl Setups {
    pub fn new(connections: Arc<Connections>, callers: Arc<dyn Callers>) -> Self {
        Self {
            connections,
            callers,
            setups: Mutex::new(HashMap::new()),
        }
    }

    fn with<T>(
        &self,
        caller: &Caller,
        id: &str,
        f: impl FnOnce(&mut Setup) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut setups = self.setups.lock().expect("setups poisoned");
        let now = Instant::now();
        setups.retain(|_, s| s.until > now);
        let gone = || {
            "There's no such setup: it ended, or it is another conversation's. Start one with connections.setup.".to_string()
        };
        let setup = setups.get_mut(id).ok_or_else(gone)?;
        if setup.person != caller.person
            || setup.by != caller.agent
            || setup.session != caller.session
        {
            return Err(gone());
        }
        f(setup)
    }

    fn check_caller(&self, caller: &Caller) -> Result<(), String> {
        if caller.session.is_none() {
            return Err("Connections are set up in a conversation with the person.".into());
        }
        match self.callers.holds_the_hub(&caller.person) {
            true => Ok(()),
            false => Err(
                "Only the person whose Desk this is sets up its connections, in their own conversation.".into(),
            ),
        }
    }

    /// Starts a setup of `service`, named `name`, for `whom` (`me`, `all`,
    /// or an agent).
    pub async fn start(
        &self,
        caller: &Caller,
        service: &str,
        name: Option<&str>,
        whom: Option<&str>,
    ) -> Result<SetupView, String> {
        self.check_caller(caller)?;
        let c = catalog::find(service)
            .ok_or_else(|| format!("Desk has no connector {service}. It sets up: Jira, Linear, Datadog, Metabase and GitHub."))?;
        let Some(recipe) = recipe(c.id) else {
            return Err(by_hand(c.id).into());
        };
        let (level, for_agent) = match whom.map(str::trim).filter(|w| !w.is_empty()) {
            None | Some("me") => (
                Level::Agent(caller.agent.clone()),
                Some(caller.agent.clone()),
            ),
            Some("all") if caller.overseer => (Level::All, None),
            Some("all") => {
                return Err(
                    "A connection for all the person's agents is the person's overseer's to set up, or the person's in Settings → Connections. Set one up just for yourself instead."
                        .into(),
                )
            }
            Some(other) => {
                let agent = self.callers.agent_named(other).ok_or_else(|| {
                    format!("There's no agent {other}. Say `me`, `all`, or an agent's name.")
                })?;
                if agent != caller.agent && !caller.overseer {
                    return Err(
                        "An agent sets up a connection just for itself. One for all the person's agents, or for another agent, is the person's overseer's to set up, or the person's in Settings → Connections."
                            .into(),
                    );
                }
                (Level::Agent(agent.clone()), Some(agent))
            }
        };
        // A name an agent chose reaches other agents' words about their
        // connections: plain characters only.
        let name: Option<String> = name.map(|n| {
            n.chars()
                .filter(|ch| ch.is_alphanumeric() || " .-_'&()@".contains(*ch))
                .collect::<String>()
        });
        let name = Connections::name(c, name.as_deref())
            .map_err(refused)?
            .unwrap_or_else(|| c.name.to_string());
        // Room for it, before anything starts (GitHub's sign-in).
        let full = || {
            let mut setups = self.setups.lock().expect("setups poisoned");
            let now = Instant::now();
            setups.retain(|_, s| s.until > now);
            setups
                .values()
                .filter(|s| s.person == caller.person)
                .count()
                >= MAX_SETUPS
        };
        if full() {
            return Err("Too many setups are under way. Finish one first.".into());
        }
        self.connections.migrate(&caller.person).await;
        let key = match c.id {
            "github" => self.connections.new_key(c, &caller.person, &name),
            _ if self.connections.keys_in_use(c, &caller.person).is_empty() => {
                catalog::DEFAULT_INSTANCE.to_string()
            }
            _ => self.connections.new_key(c, &caller.person, &name),
        };
        let whose = match &for_agent {
            Some(a) => self
                .connections
                .agents
                .get(a)
                .map_or_else(|| a.clone(), |a| a.name),
            None => "all your agents".into(),
        };
        let mut steps = Vec::new();
        if let Some(label) = recipe.where_label {
            steps.push(step("where", label));
        }
        // GitHub's own sign-in, where Desk's GitHub app is set up: the
        // person approves a code, and nothing is captured.
        let device = c.id == "github" && self.connections.github_client_id.is_some();
        let mut flow = None;
        if device {
            let started = self
                .connections
                .begin_github(
                    &caller.person,
                    c,
                    level.clone(),
                    Some(key.clone()),
                    Some(name.clone()),
                )
                .await
                .map_err(refused)?;
            steps.push(step(
                "page",
                &format!(
                    "Open GitHub's device sign-in: {}",
                    started.verification_uri.clone().unwrap_or_default()
                ),
            ));
            steps.push(step(
                "sign_in",
                &format!(
                    "Sign in to GitHub and enter the code {}",
                    started.user_code.clone().unwrap_or_default()
                ),
            ));
            flow = Some(started.flow);
        } else {
            steps.push(step("page", recipe.page_label));
            steps.push(step("sign_in", recipe.sign_in_label));
            steps.push(step("key", recipe.key_label));
        }
        steps.push(step(
            "check",
            &format!("Check it works, then turn it on for {whose}"),
        ));
        steps[0].state = StepState::Current;
        let setup = Setup {
            id: uuid::Uuid::new_v4().simple().to_string(),
            person: caller.person.clone(),
            by: caller.agent.clone(),
            session: caller.session.clone(),
            service: c.id,
            name,
            key,
            level,
            for_agent,
            steps,
            fields: BTreeMap::new(),
            secrets: BTreeMap::new(),
            flow,
            read_at: None,
            started_at: Utc::now(),
            until: Instant::now() + SETUP_TTL,
            connected: None,
        };
        let view = setup.view();
        let mut setups = self.setups.lock().expect("setups poisoned");
        let now = Instant::now();
        setups.retain(|_, s| s.until > now);
        if setups
            .values()
            .filter(|s| s.person == caller.person)
            .count()
            >= MAX_SETUPS
        {
            return Err("Too many setups are under way. Finish one first.".into());
        }
        setups.insert(setup.id.clone(), setup);
        Ok(view)
    }

    /// What the agent does next, as `connections.setup` tells it.
    pub fn next(&self, view: &SetupView) -> String {
        let c = catalog::find(view.service).expect("in the catalog");
        let page = recipe(view.service).and_then(|r| (r.page)(&BTreeMap::new()));
        let secrets: Vec<&str> = c
            .fields
            .iter()
            .filter(|f| f.secret)
            .map(|f| f.name)
            .collect();
        let plain: Vec<&str> = c
            .fields
            .iter()
            .filter(|f| !f.secret)
            .map(|f| f.name)
            .collect();
        if view
            .steps
            .iter()
            .any(|s| s.label.contains("device sign-in"))
        {
            return "Open the device sign-in page in your browser and tell the person the code; they sign in and approve it. Call connections.test once they say they have. Never type their password.".into();
        }
        format!(
            "1. Keep {} with connections.keep (ask the person if you can't tell).\n\
             2. Open {} in your browser. If it asks to sign in, mark step sign_in waiting with connections.step and tell the person: they sign in themselves. Never type or read a password.\n\
             3. Once signed in, make the key (name it like \"Desk · {}\") with browser.click. The page hides keys from you: read {} with browser.capture_key (the element that shows it, and this service), which keeps it for this setup. The key never comes back to you.\n\
             4. Check it with connections.test (the person allows it), then connections.use to turn it on.",
            if plain.is_empty() { "nothing".to_string() } else { plain.join(", ") },
            page.unwrap_or_else(|| "the service's key page".into()),
            view.name,
            secrets.join(" and "),
        )
    }

    pub fn step(
        &self,
        caller: &Caller,
        id: &str,
        step: &str,
        state: StepState,
        note: Option<String>,
    ) -> Result<SetupView, String> {
        self.with(caller, id, |s| {
            if !s.steps.iter().any(|x| x.id == step) {
                let ids: Vec<&str> = s.steps.iter().map(|x| x.id).collect();
                return Err(format!("This setup's steps are {}", ids.join(", ")));
            }
            // What the hub checks is the hub's to mark.
            if step == "check" && state == StepState::Done {
                return Err("connections.test marks the check done.".into());
            }
            let note = note.map(|n| n.chars().take(200).collect::<String>());
            s.mark(step, state, note);
            Ok(s.view())
        })
    }

    pub fn keep(
        &self,
        caller: &Caller,
        id: &str,
        field: &str,
        value: &str,
    ) -> Result<SetupView, String> {
        self.with(caller, id, |s| {
            let c = catalog::find(s.service).expect("in the catalog");
            let f = c
                .fields
                .iter()
                .find(|f| f.name == field)
                .ok_or_else(|| format!("{} has no field {field}", c.name))?;
            if f.secret {
                return Err(format!(
                    "{} is a secret: read it off the service's page with browser.capture_key, so it never passes through you.",
                    f.label
                ));
            }
            // A key held goes only where it was read for: nothing about
            // the connection changes once one is.
            if !s.secrets.is_empty() {
                return Err(
                    "A key is held for this setup already, so its details can't change now. Start a new setup to change them."
                        .into(),
                );
            }
            let value = value.trim();
            if value.is_empty() || value.len() > MAX_FIELD || value.chars().any(char::is_control) {
                return Err(format!("{} should be one short line", f.label));
            }
            let value = match (s.service, field) {
                ("jira", "site") | ("metabase", "site_url") => site_origin(s.service, value)?,
                ("datadog", "site") => match DATADOG_SITES.contains(&value) {
                    true => value.to_string(),
                    false => {
                        return Err(format!(
                            "Datadog's site is one of {}",
                            DATADOG_SITES.join(", ")
                        ))
                    }
                },
                _ => value.to_string(),
            };
            let value = value.as_str();
            s.fields.insert(field.to_string(), value.to_string());
            if Some(field) == c.site {
                s.relabel("where", format!("Found your site: {value}"));
                s.mark("where", StepState::Done, None);
            }
            if field == "email" {
                let label = s
                    .steps
                    .iter()
                    .find(|x| x.id == "sign_in")
                    .map(|x| x.label.clone())
                    .unwrap_or_default();
                if !label.contains(" as ") {
                    s.relabel("sign_in", format!("{label} as {value}"));
                }
            }
            Ok(s.view())
        })
    }

    /// The setup `caller`'s conversation is running for `connector`, still
    /// waiting on a key: its id, as [`Self::with`] finds it.
    fn running(&self, caller: &Caller, connector: &str) -> Option<String> {
        let mut setups = self.setups.lock().expect("setups poisoned");
        let now = Instant::now();
        setups.retain(|_, s| s.until > now);
        setups
            .values()
            .filter(|s| {
                s.person == caller.person
                    && s.by == caller.agent
                    && s.session == caller.session
                    && s.service == connector
                    && s.connected.is_none()
                    && s.flow.is_none()
            })
            .max_by_key(|s| s.started_at)
            .map(|s| s.id.clone())
    }

    /// The page `connector`'s key is read off, for the setup `caller`'s
    /// conversation is running for it: `None` when there is none, and the
    /// browser reads nothing.
    pub fn key_page(&self, caller: &Caller, connector: &str) -> Option<KeyPage> {
        self.check_caller(caller).ok()?;
        let id = self.running(caller, connector)?;
        self.page_of(caller, &id, connector)
    }

    /// The key page of the setup `id`, for `connector`.
    fn page_of(&self, caller: &Caller, id: &str, connector: &str) -> Option<KeyPage> {
        let recipe = recipe(connector)?;
        self.with(caller, id, |s| {
            let origin = (recipe.origins)(&s.fields)
                .into_iter()
                .next()
                .ok_or_else(String::new)?;
            let c = catalog::find(s.service).expect("in the catalog");
            Ok(KeyPage {
                origin,
                paths: recipe.paths.to_vec(),
                workspace_segment: recipe.workspace_segment,
                fields: c
                    .fields
                    .iter()
                    .filter(|f| f.secret && !s.secrets.contains_key(f.name))
                    .map(|f| f.name)
                    .collect(),
            })
        })
        .ok()
    }

    /// The caller of the turn running now, as the browser names it: `user`'s
    /// `agent` in the conversation whose session key is `conversation`.
    /// `None` outside a conversation, or when the turn's own person isn't
    /// `user` or its agent isn't `agent`, so the browser's word alone never
    /// makes a caller.
    pub fn turn_caller(&self, user: &str, agent: &str, conversation: &str) -> Option<Caller> {
        use workforce_agent::runner::{
            CURRENT_AGENT_ID, CURRENT_SESSION_ID, CURRENT_SESSION_KEY, CURRENT_USER_ID,
        };
        let person = CURRENT_USER_ID.try_with(|u| u.clone()).ok()?;
        let key = CURRENT_SESSION_KEY.try_with(|k| k.clone()).ok()?;
        let turn_agent = CURRENT_AGENT_ID.try_with(|a| a.clone()).ok()?;
        if person != user || turn_agent != agent || key != conversation || conversation.is_empty() {
            return None;
        }
        let session = CURRENT_SESSION_ID
            .try_with(|s| s.clone())
            .ok()
            .filter(|s| !s.is_empty())?;
        Some(Caller {
            overseer: self.callers.oversees(agent),
            person,
            agent: agent.to_string(),
            session: Some(session),
            session_key: Some(key),
        })
    }

    /// [`Self::key_page`] as the browser takes it: what `browser.capture_key`
    /// checks the tab against and sends the extension's read. Every key page
    /// is registered at install ([`register_key_pages`]), so its text is
    /// hidden and it is never pictured.
    pub fn browser_page(&self, caller: &Caller, connector: &str) -> Option<browser_keys::KeyPage> {
        let page = self.key_page(caller, connector)?;
        browser_page(connector, &page.origin)
    }

    /// Keeps `value`, a key the browser read off `read_origin` and
    /// `read_path` for the setup `caller`'s conversation is running for
    /// `connector`: its `field` (the next one still to read where none is
    /// named). The page must be the setup's key page, and the value shaped
    /// like that service's key; otherwise it is dropped. Nothing is sent
    /// anywhere: once every key is held, the agent checks it with
    /// `connections.test`, which asks the person first. What comes back
    /// holds none of it.
    pub fn keep_key(
        &self,
        caller: &Caller,
        connector: &str,
        field: Option<&str>,
        read_origin: &str,
        read_path: &str,
        value: Secret<String>,
    ) -> Result<KeptKey, String> {
        self.check_caller(caller)?;
        let id = self.running(caller, connector).ok_or_else(|| {
            format!("No {connector} setup is waiting on a key in this conversation, so nothing was kept. Start one with connections.setup.")
        })?;
        let recipe = recipe(connector).ok_or_else(|| by_hand(connector).to_string())?;
        let page = self
            .page_of(caller, &id, connector)
            .ok_or("That setup has ended.")?;
        if read_origin != page.origin || !page.holds(read_path) {
            return Err("That wasn't the service's own key page, so nothing was kept.".into());
        }
        let c = catalog::find(connector).expect("in the catalog");
        let field = match field {
            Some(f) => c
                .fields
                .iter()
                .find(|x| x.name == f && x.secret)
                .map(|x| x.name)
                .ok_or_else(|| format!("{} has no key named {f}", c.name))?,
            // The first still to read that it is shaped for: Datadog's API
            // and application keys may be read in either order.
            None => {
                let first = *page
                    .fields
                    .first()
                    .ok_or("Every key this setup needs is held already.")?;
                page.fields
                    .iter()
                    .copied()
                    .find(|f| (recipe.shaped)(f, value.expose_secret()))
                    .unwrap_or(first)
            }
        };
        if !(recipe.shaped)(field, value.expose_secret()) {
            return Err(
                "What was read doesn't look like this service's key, so nothing was kept. Point at the key itself."
                    .into(),
            );
        }
        // Should it come back into view, it reaches the model as a marker.
        if let (Some(cache), Some(key)) = (
            workforce_common::secret_marker::global_cache(),
            caller.session_key.as_deref(),
        ) {
            cache.capture(
                key,
                Secret::new(value.expose_secret().clone()),
                "CONNECTION_KEY",
                workforce_common::secret_marker::SecretSource::ToolResult {
                    tool: "browser.capture_key".into(),
                },
            );
        }
        let length = value.expose_secret().chars().count();
        let all = self.with(caller, &id, move |s| {
            s.secrets.insert(field.to_string(), value);
            s.read_at = Some(page.origin.clone());
            let all = c
                .fields
                .iter()
                .filter(|f| f.secret)
                .all(|f| s.secrets.contains_key(f.name));
            if all {
                for step in ["page", "sign_in", "key"] {
                    s.mark(step, StepState::Done, None);
                }
            }
            Ok(all)
        })?;
        let test = match all {
            false => {
                let left = self
                    .key_page(caller, connector)
                    .map(|p| p.fields)
                    .unwrap_or_default();
                format!("still needs {}", left.join(" and "))
            }
            true => format!("ready: check it with connections.test (setup {id})"),
        };
        Ok(KeptKey {
            kept: true,
            connector: c.id,
            field,
            length,
            test,
        })
    }

    /// Checks what the setup holds and, on a pass, keeps it at its tier.
    pub async fn test(&self, caller: &Caller, id: &str) -> Result<SetupView, String> {
        self.check_caller(caller)?;
        let (service, level, key, name, fields, flow, read_at) = self.with(caller, id, |s| {
            let mut fields: BTreeMap<String, String> = s.fields.clone();
            for (k, v) in &s.secrets {
                fields.insert(k.clone(), v.expose_secret().clone());
            }
            Ok((
                s.service,
                s.level.clone(),
                s.key.clone(),
                s.name.clone(),
                fields,
                s.flow.clone(),
                s.read_at.clone(),
            ))
        })?;
        // Where the key goes: the site kept for it, checked again, and
        // Metabase's only where its key was read.
        if flow.is_none() {
            for (svc, f) in [("jira", "site"), ("metabase", "site_url")] {
                if service == svc {
                    let site = fields
                        .get(f)
                        .ok_or("Keep the site with connections.keep first.")?;
                    let origin = site_origin(svc, site)?;
                    if svc == "metabase" && read_at.as_deref() != Some(origin.as_str()) {
                        return Err("Metabase's key was read on another site than the one kept, so nothing was kept.".into());
                    }
                }
            }
            if !catalog::find(service)
                .expect("in the catalog")
                .fields
                .iter()
                .filter(|f| f.secret)
                .all(|f| fields.contains_key(f.name))
            {
                return Err("Read the key with browser.capture_key first.".into());
            }
        }
        let c = catalog::find(service).expect("in the catalog");
        let person = caller.person.clone();
        let (account, conn) = match flow {
            Some(flow) => match self.connections.flow(&person, &flow) {
                Some(super::FlowState::Connected { account }) => (
                    account,
                    Conn {
                        level: level.clone(),
                        key: key.clone(),
                    },
                ),
                Some(super::FlowState::Waiting) => {
                    return Err(
                        "GitHub is still waiting for the person to approve the code.".into(),
                    )
                }
                other => {
                    let said = match other {
                        Some(super::FlowState::Failed { error }) => error,
                        _ => "The sign-in ended without connecting".into(),
                    };
                    self.with(caller, id, |s| {
                        s.mark("check", StepState::Failed, Some(said.clone()));
                        Ok(())
                    })?;
                    return Err(format!("{said}. Start again with connections.setup."));
                }
            },
            None => {
                let fields = Connections::fields(c, fields).map_err(refused)?;
                // Another connection may have taken its key meanwhile.
                let key = match self.connections.keys_in_use(c, &person).contains(&key) {
                    true => self.connections.new_key(c, &person, &name),
                    false => key,
                };
                let account = match self.connections.check(c, &fields).await {
                    Found::Works { account } => account,
                    found @ (Found::Refused(_) | Found::Failed(_)) => {
                        // The service's own words go on the person's card,
                        // never back to the agent.
                        let (said, told) = match &found {
                            Found::Refused(e) => (e, format!("{} didn't accept the key.", c.name)),
                            Found::Failed(e) => (e, format!("{} couldn't be reached.", c.name)),
                            Found::Works { .. } => unreachable!(),
                        };
                        let note: String = super::scrub(said, &fields).chars().take(200).collect();
                        self.with(caller, id, |s| {
                            s.secrets.clear();
                            s.read_at = None;
                            s.mark("check", StepState::Failed, Some(note));
                            Ok(())
                        })?;
                        return Err(format!(
                            "{told} Nothing was kept. Make a new key and read it again."
                        ));
                    }
                };
                let conn = Conn {
                    level: level.clone(),
                    key,
                };
                self.connections
                    .keep(c, &conn.level, &conn.key, &person, &fields, Some(&name))
                    .map_err(refused)?;
                self.connections
                    .record(
                        c,
                        &conn,
                        &person,
                        &Found::Works {
                            account: account.clone(),
                        },
                        true,
                    )
                    .await;
                self.connections.reload(c, &conn.level, &person).await;
                (account, conn)
            }
        };
        self.with(caller, id, |s| {
            s.key = conn.key.clone();
            s.secrets.clear();
            s.connected = Some(Connected {
                connection: conn.id(),
                account: account.clone(),
                on_for: None,
                write: None,
            });
            for step in ["where", "page", "sign_in", "key"] {
                s.mark(step, StepState::Done, None);
            }
            s.mark(
                "check",
                StepState::Current,
                Some(format!("Connected as {account}")),
            );
            Ok(s.view())
        })
    }

    /// Turns connection `which` on for `agent` (the caller where none).
    #[allow(clippy::too_many_arguments)]
    pub async fn use_it(
        &self,
        caller: &Caller,
        setup: Option<&str>,
        service: Option<&str>,
        connection: Option<&str>,
        agent: Option<&str>,
        write: bool,
        default: bool,
    ) -> Result<String, String> {
        self.check_caller(caller)?;
        let (c, conn, planned) = match setup {
            Some(id) => self.with(caller, id, |s| {
                let conn = s
                    .connected
                    .as_ref()
                    .ok_or("Call connections.test first: it isn't connected yet.")?
                    .connection
                    .clone();
                Ok((
                    catalog::find(s.service).expect("in the catalog"),
                    conn,
                    s.for_agent.clone(),
                ))
            })?,
            None => {
                let c = service.and_then(catalog::find).ok_or(
                    "Say which service (`service`) and connection (`connection`), or the setup.",
                )?;
                let named = connection.ok_or("Say which connection (`connection`).")?;
                let conn = self.find(c, &caller.person, named)?;
                (c, conn, None)
            }
        };
        let target = match agent.map(str::trim).filter(|a| !a.is_empty()) {
            None | Some("me") => planned.unwrap_or_else(|| caller.agent.clone()),
            Some(name) => self
                .callers
                .agent_named(name)
                .ok_or_else(|| format!("There's no agent {name}"))?,
        };
        if target != caller.agent && !caller.overseer {
            return Err(
                "An agent turns a connection on for itself only. For another agent, the person's overseer does it, or the person in that agent's Connectors."
                    .into(),
            );
        }
        let services = self
            .connections
            .agent_picks(&caller.person, &target)
            .await
            .map_err(refused)?;
        let now = services
            .iter()
            .find(|s| s.id == c.id)
            .ok_or("That service isn't one of the agent's")?;
        if !now.connections.iter().any(|p| p.id == conn) {
            return Err(format!(
                "That {} connection is another agent's own, so it can't be turned on here. Set one up for this agent.",
                c.name
            ));
        }
        let mut uses: Vec<super::picks::Use> = now
            .connections
            .iter()
            .filter(|p| p.picked && p.id != conn)
            .map(|p| super::picks::Use {
                id: p.id.clone(),
                write: p.write,
            })
            .collect();
        uses.push(super::picks::Use {
            id: conn.clone(),
            write,
        });
        let default = match default {
            true => Some(conn.clone()),
            false => now
                .connections
                .iter()
                .find(|p| p.default && p.picked)
                .map(|p| p.id.clone())
                .or_else(|| Some(conn.clone())),
        };
        self.connections
            .set_picks(
                &caller.person,
                &target,
                super::picks::PicksBody {
                    service: c.id.to_string(),
                    connections: uses,
                    default,
                },
            )
            .await
            .map_err(refused)?;
        let name = self
            .connections
            .agents
            .get(&target)
            .map_or_else(|| target.clone(), |a| a.name);
        if let Some(id) = setup {
            let _ = self.with(caller, id, |s| {
                if let Some(done) = s.connected.as_mut() {
                    done.on_for = Some(name.clone());
                    done.write = Some(write);
                }
                s.mark("check", StepState::Done, None);
                Ok(())
            });
        }
        Ok(format!(
            "{} is on for {name}, {}.",
            c.name,
            match write {
                true => "read and write",
                false => "read only",
            }
        ))
    }

    /// The connection of `c` the person calls `named` (its name or key).
    fn find(&self, c: &Connector, person: &str, named: &str) -> Result<String, String> {
        let wanted = named.trim().to_lowercase();
        let mut found = Vec::new();
        for level in self.connections.levels() {
            for (key, tier) in self.connections.instances(c, &level, person) {
                let (name, _) = self.connections.named_at(c, &level, &key, tier, person);
                if key == wanted || name.is_some_and(|n| n.to_lowercase() == wanted) {
                    found.push(
                        Conn {
                            level: level.clone(),
                            key,
                        }
                        .id(),
                    );
                }
            }
        }
        match found.len() {
            1 => Ok(found.remove(0)),
            0 => Err(format!("There's no {} connection named {named}", c.name)),
            _ => Err(format!(
                "Several {} connections are named {named}: use the setup's id instead",
                c.name
            )),
        }
    }

    /// Whom setup `id` keeps its connection for, and where its key goes,
    /// for the gate's question: `None` for no such setup.
    fn described(&self, id: &str) -> Option<Described> {
        let setups = self.setups.lock().expect("setups poisoned");
        let s = setups.get(id)?;
        let c = catalog::find(s.service)?;
        // Where its key goes, as the hub checks it: never the words it
        // was kept with.
        let host = recipe(s.service)
            .and_then(|r| match s.service {
                "jira" => s
                    .fields
                    .get("site")
                    .and_then(|f| site_origin("jira", f).ok()),
                "metabase" => s
                    .fields
                    .get("site_url")
                    .and_then(|f| site_origin("metabase", f).ok()),
                "datadog" => Some(datadog_web(&s.fields)),
                _ => (r.origins)(&s.fields).into_iter().next(),
            })
            .map(|h| h.trim_start_matches("https://").to_string());
        let whom = match &s.for_agent {
            Some(a) => self.agent_name(a),
            None => "all the person's agents".into(),
        };
        Some(Described {
            service: c.name,
            name: s.name.clone(),
            host,
            whom,
            for_agent: s.for_agent.clone(),
        })
    }

    /// Agent `id`'s name as the hub has it; `unknown` for none.
    fn agent_name(&self, id: &str) -> String {
        self.connections
            .agents
            .get(id)
            .map_or_else(|| UNKNOWN.to_string(), |a| a.name)
    }

    /// What `connections.use` named, as the hub resolves it, for the
    /// gate's question: the connection (`Linear · acme`) and the agent;
    /// `unknown` for what doesn't resolve.
    pub(super) fn use_described(
        &self,
        caller: Option<&Caller>,
        service: Option<&str>,
        connection: Option<&str>,
        agent: Option<&str>,
    ) -> (String, String) {
        let c = service.and_then(catalog::find);
        let what = match (c, caller, connection) {
            (Some(c), Some(caller), Some(named)) => match self.find(c, &caller.person, named) {
                Ok(id) => format!("{} · {}", c.name, id.rsplit('~').next().unwrap_or(UNKNOWN)),
                Err(_) => format!("{} · {UNKNOWN}", c.name),
            },
            (Some(c), _, _) => format!("{} · {UNKNOWN}", c.name),
            _ => UNKNOWN.to_string(),
        };
        let whom = match agent.map(str::trim).filter(|a| !a.is_empty()) {
            None | Some("me") => match caller {
                Some(caller) => self.agent_name(&caller.agent),
                None => UNKNOWN.into(),
            },
            Some(name) => self
                .callers
                .agent_named(name)
                .map_or_else(|| UNKNOWN.to_string(), |id| self.agent_name(&id)),
        };
        (what, whom)
    }

    /// What `connections.keep` would keep, as the hub resolves it, for the
    /// gate's question: the field's label and, for where the key goes, the
    /// address it checks; any other value by its length.
    pub(super) fn keep_described(
        &self,
        id: &str,
        field: Option<&str>,
        value: Option<&str>,
    ) -> String {
        let setups = self.setups.lock().expect("setups poisoned");
        let Some(s) = setups.get(id) else {
            return UNKNOWN.into();
        };
        let Some(f) = catalog::find(s.service)
            .and_then(|c| c.fields.iter().find(|f| Some(f.name) == field && !f.secret))
        else {
            return UNKNOWN.into();
        };
        let value = value.unwrap_or_default();
        let shown = match (s.service, f.name) {
            ("jira", "site") | ("metabase", "site_url") => site_origin(s.service, value)
                .map_or_else(
                    |_| UNKNOWN.to_string(),
                    |o| o.trim_start_matches("https://").to_string(),
                ),
            ("datadog", "site") => {
                let mut fields = BTreeMap::new();
                fields.insert("site".to_string(), value.to_string());
                match DATADOG_SITES.contains(&value.trim()) {
                    true => datadog_web(&fields)
                        .trim_start_matches("https://")
                        .to_string(),
                    false => UNKNOWN.into(),
                }
            }
            _ => format!("{} characters", value.chars().count()),
        };
        format!("{}: {shown}", f.label)
    }

    /// The person's setups, for the card: every one of `agent`'s, or all.
    pub fn views(&self, person: &str, agent: Option<&str>) -> Vec<SetupView> {
        let mut setups = self.setups.lock().expect("setups poisoned");
        let now = Instant::now();
        setups.retain(|_, s| s.until > now);
        let mut views: Vec<SetupView> = setups
            .values()
            .filter(|s| s.person == person && agent.is_none_or(|a| s.by == a))
            .map(Setup::view)
            .collect();
        views.sort_by_key(|v| v.started_at);
        views
    }

    /// The person signed in for setup `id`, from its card.
    pub fn signed_in(&self, person: &str, id: &str) -> Option<SetupView> {
        let mut setups = self.setups.lock().expect("setups poisoned");
        let now = Instant::now();
        setups.retain(|_, s| s.until > now);
        let s = setups.get_mut(id).filter(|s| s.person == person)?;
        if s.steps
            .iter()
            .any(|x| x.id == "sign_in" && x.state != StepState::Done)
        {
            s.mark(
                "sign_in",
                StepState::Done,
                Some("You said you're signed in".into()),
            );
        }
        Some(s.view())
    }

    /// Listing what the agent may use, for `connections.list`.
    pub async fn listing(&self, caller: &Caller) -> Result<Value, String> {
        self.check_caller(caller)?;
        let services = self
            .connections
            .agent_picks(&caller.person, &caller.agent)
            .await
            .map_err(refused)?;
        Ok(json!({
            "services": services.iter().map(|s| json!({
                "service": s.id,
                "name": s.name,
                "setUpByAgent": recipe(s.id).is_some(),
                "connections": s.connections.iter().map(|p| json!({
                    "name": p.name,
                    "instance": p.id.rsplit('~').next(),
                    "site": p.site,
                    "account": p.account,
                    "for": if p.level == "all" { "all the person's agents" } else { "just you" },
                    "status": p.status,
                    "youUseIt": p.picked,
                    "write": p.write,
                    "default": p.default,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }))
    }
}

/// A setup, as the gate's question says it.
struct Described {
    service: &'static str,
    name: String,
    host: Option<String>,
    whom: String,
    /// `None`: for all the person's agents.
    for_agent: Option<String>,
}

fn step(id: &'static str, label: &str) -> Step {
    Step {
        id,
        label: label.to_string(),
        state: StepState::Todo,
        note: None,
    }
}

fn refused(r: Refusal) -> String {
    match r {
        Refusal::Invalid(e) | Refusal::NotFound(e) | Refusal::Unavailable(e) => e,
        Refusal::TestFailed { error, .. } => error,
        Refusal::Internal(e) => {
            tracing::error!(error = %format!("{e:#}"), "a connection setup failed");
            "Desk couldn't do that. Try again.".into()
        }
    }
}

static INSTALLED: OnceLock<Arc<Setups>> = OnceLock::new();

/// The hub's setups, once a `[desk]` hub mounted Connections.
pub fn installed() -> Option<Arc<Setups>> {
    INSTALLED.get().cloned()
}

/// The hub's callers, from its registry and identities.
struct Hub(Arc<crate::gateway::GatewayState>);

impl Callers for Hub {
    fn holds_the_hub(&self, person: &str) -> bool {
        crate::desk_floor::holds_the_hub(self.0.identity_store.as_ref(), true, Some(person))
    }

    fn oversees(&self, agent: &str) -> bool {
        self.0.registry.get(agent).is_some_and(|service| {
            crate::desk_crew::for_service(&service)
                .is_some_and(|crew| crew.overseer_named(agent).is_some())
        })
    }

    fn agent_named(&self, name: &str) -> Option<String> {
        let wanted = name.trim().to_lowercase();
        self.0
            .registry
            .all_services()
            .iter()
            .find(|s| {
                s.agent_id.as_str().eq_ignore_ascii_case(&wanted)
                    || crate::desk_crew::name_of(s).to_lowercase() == wanted
            })
            .map(|s| s.agent_id.as_str().to_string())
    }
}

/// Setups over `connections`, and their routes.
pub fn install(state: &Arc<crate::gateway::GatewayState>, connections: Arc<Connections>) -> Router {
    let setups = INSTALLED
        .get_or_init(|| Arc::new(Setups::new(connections, Arc::new(Hub(Arc::clone(state))))))
        .clone();
    register_key_pages();
    Router::new()
        .route("/api/desk/connection-setups", get(list_route))
        .route(
            "/api/desk/connection-setups/{id}/signed-in",
            post(signed_in_route),
        )
        .with_state(Arc::new(Routes {
            state: Arc::clone(state),
            setups,
        }))
}

struct Routes {
    state: Arc<crate::gateway::GatewayState>,
    setups: Arc<Setups>,
}

#[derive(Deserialize)]
struct ListQuery {
    token: Option<String>,
    agent: Option<String>,
}

async fn holder(
    routes: &Routes,
    headers: &HeaderMap,
    token: Option<&str>,
) -> Result<String, Response> {
    let caller = crate::gateway::resolve_caller_context(&routes.state, headers, token).await;
    let refuse =
        |status: StatusCode, e: &str| (status, Json(json!({ "error": e }))).into_response();
    if caller.auth_failure.is_some() {
        return Err(refuse(StatusCode::UNAUTHORIZED, "not allowed"));
    }
    match caller.identity_id {
        Some(person) if routes.setups.callers.holds_the_hub(&person) => Ok(person),
        _ => Err(refuse(
            StatusCode::FORBIDDEN,
            "Only the person whose Desk this is sees its setups",
        )),
    }
}

async fn list_route(
    State(r): State<Arc<Routes>>,
    h: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Response {
    match holder(&r, &h, q.token.as_deref()).await {
        Ok(person) => {
            Json(json!({ "setups": r.setups.views(&person, q.agent.as_deref()) })).into_response()
        }
        Err(refused) => refused,
    }
}

async fn signed_in_route(
    State(r): State<Arc<Routes>>,
    h: HeaderMap,
    Query(q): Query<ListQuery>,
    Path(id): Path<String>,
) -> Response {
    let person = match holder(&r, &h, q.token.as_deref()).await {
        Ok(person) => person,
        Err(refused) => return refused,
    };
    match r.setups.signed_in(&person, &id) {
        Some(view) => Json(json!({ "setup": view })).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "That setup has ended. Ask the agent to start again." })),
        )
            .into_response(),
    }
}

/// One `connections.*` tool, for the agent `agent`.
pub struct SetupTool {
    name: &'static str,
    agent: String,
}

impl SetupTool {
    pub fn all(agent: &str) -> Vec<Self> {
        NAMES
            .iter()
            .map(|name| Self {
                name,
                agent: agent.to_string(),
            })
            .collect()
    }

    fn caller(&self, setups: &Setups) -> Option<Caller> {
        use workforce_agent::runner::{CURRENT_SESSION_ID, CURRENT_SESSION_KEY, CURRENT_USER_ID};
        let person = CURRENT_USER_ID.try_with(|u| u.clone()).ok()?;
        // Only in a conversation: a job or a sessionless call has none.
        let session = CURRENT_SESSION_ID
            .try_with(|s| s.clone())
            .ok()
            .filter(|s| !s.is_empty())?;
        Some(Caller {
            overseer: setups.callers.oversees(&self.agent),
            person,
            agent: self.agent.clone(),
            session: Some(session),
            session_key: CURRENT_SESSION_KEY
                .try_with(|s| s.clone())
                .ok()
                .filter(|s| !s.is_empty()),
        })
    }
}

fn text(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[async_trait]
impl Tool for SetupTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        match self.name {
            "connections.list" => "The person's connections to services (Jira, Linear, GitHub, Datadog, Metabase, Email, Google, Slack) you can use: each by the name the person gave it, its site and account, whether it is for all their agents or just you, whether you use it, take writes with it, and which is your default. Never a key.",
            "connections.step" => "Mark a step of a setup's card: `current` (you're on it), `waiting` (on the person: signing in, approving a code), or `done`. The person sees the card in this conversation.",
            "connections.setup" => "Start setting up a connection to a service yourself, in your browser: Jira, Linear, Datadog, Metabase or GitHub. `name` is what the person calls it (Acme, Work). `for` is `me` (default), or, for the person's overseer only, `all` or another agent's name. Returns the setup's id, its steps, and what to do next. Google, Email and Slack the person connects themselves.",
            "connections.keep" => "Keep a field of a setup that isn't a secret: a Jira site (acme.atlassian.net), a Metabase address (its https origin), a Datadog site (us1, eu1, …), an email. A key is read off its page with browser.capture_key instead, and once one is held nothing more is kept.",
            "connections.test" => "Check the setup's keys with the service again, or finish GitHub's sign-in once the person approved its code. Only if it works are they kept, in the vault, for whom the setup is for. A failed check keeps nothing.",
            "connections.use" => "Turn a connection on for an agent: the setup's (`setup`), or one by `service` and `connection` (its name). `agent` is you by default; another agent only for the person's overseer. `write` (default false) lets it make changes; `default` makes it the one used when a request doesn't say.",
            _ => "",
        }
    }

    fn input_schema(&self) -> Value {
        let setup =
            json!({ "type": "string", "description": "The setup's id, from connections.setup" });
        match self.name {
            "connections.list" => json!({ "type": "object", "properties": {} }),
            "connections.step" => json!({
                "type": "object",
                "properties": {
                    "setup": setup,
                    "step": { "type": "string", "description": "The step's id: where, page, sign_in, key or check" },
                    "state": { "type": "string", "enum": ["current", "waiting", "done"] },
                    "note": { "type": "string", "description": "A few words for the person" }
                },
                "required": ["setup", "step", "state"]
            }),
            "connections.setup" => json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string", "enum": ["jira", "linear", "datadog", "metabase", "github"] },
                    "name": { "type": "string" },
                    "for": { "type": "string", "description": "me (default), all, or an agent's name" }
                },
                "required": ["service"]
            }),
            "connections.keep" => json!({
                "type": "object",
                "properties": {
                    "setup": setup,
                    "field": { "type": "string", "description": "site, email, site_url" },
                    "value": { "type": "string" }
                },
                "required": ["setup", "field", "value"]
            }),
            "connections.test" => json!({
                "type": "object",
                "properties": { "setup": setup },
                "required": ["setup"]
            }),
            "connections.use" => json!({
                "type": "object",
                "properties": {
                    "setup": setup,
                    "service": { "type": "string" },
                    "connection": { "type": "string", "description": "The connection's name" },
                    "agent": { "type": "string", "description": "me (default), or an agent's name" },
                    "write": { "type": "boolean" },
                    "default": { "type": "boolean" }
                }
            }),
            _ => json!({ "type": "object" }),
        }
    }

    /// Listing, and marking a card's step, touch nothing, nor does starting
    /// a setup (but GitHub's, which starts its sign-in). Keeping a field,
    /// testing and keeping a key, and turning a connection on are
    /// connection writes to Desk's gate: Allow once or Deny below unbound.
    /// One whose connection is for all the person's agents or another agent
    /// is `connections:other`, as the setup's own record says, not the call.
    fn effect(&self, input: &Value) -> Effect {
        let write = |other: bool| Effect::Connector {
            connector: match other {
                true => "connections:other".into(),
                false => "connections".into(),
            },
            write: true,
        };
        let described =
            text(input, "setup").and_then(|id| installed().and_then(|s| s.described(&id)));
        let for_other = described
            .as_ref()
            .is_some_and(|d| d.for_agent.as_deref() != Some(self.agent.as_str()));
        match self.name {
            "connections.list" | "connections.step" => Effect::None,
            "connections.setup" => match text(input, "service").as_deref() {
                Some("github") => {
                    write(text(input, "for").is_some_and(|f| f != "me" && f != self.agent))
                }
                _ => Effect::None,
            },
            "connections.keep" | "connections.test" => write(for_other),
            "connections.use" => write(
                for_other || text(input, "agent").is_some_and(|a| a != "me" && a != self.agent),
            ),
            _ => write(true),
        }
    }

    /// The question names what is set up, where its key goes, and for
    /// whom, from the setup's own record.
    fn approval_summary(&self, input: &Value) -> Option<String> {
        let setups = installed();
        let d = text(input, "setup").and_then(|id| setups.as_ref().and_then(|s| s.described(&id)));
        let what = |d: &Described| format!("{} · {}", d.service, d.name);
        let to = |d: &Described| d.host.clone().unwrap_or_else(|| UNKNOWN.into());
        Some(match (self.name, &d) {
            ("connections.keep", Some(d)) => format!(
                "Keep {} for {}'s setup ({})",
                setups.as_ref().map_or_else(
                    || UNKNOWN.to_string(),
                    |s| s.keep_described(
                        &text(input, "setup").unwrap_or_default(),
                        text(input, "field").as_deref(),
                        text(input, "value").as_deref(),
                    )
                ),
                what(d),
                d.whom,
            ),
            ("connections.test", Some(d)) => format!(
                "Send the new {} key to {} to check it, and keep it for {}",
                what(d),
                to(d),
                d.whom
            ),
            ("connections.use", _) => {
                let (named, whom) = setups.as_ref().map_or_else(
                    || (UNKNOWN.to_string(), UNKNOWN.to_string()),
                    |s| {
                        s.use_described(
                            self.caller(s).as_ref(),
                            text(input, "service").as_deref(),
                            text(input, "connection").as_deref(),
                            text(input, "agent").as_deref(),
                        )
                    },
                );
                format!(
                    "Turn {} on for {}{}",
                    d.as_ref().map(what).unwrap_or(named),
                    match (&d, text(input, "agent")) {
                        (Some(d), None) => d.whom.clone(),
                        _ => whom,
                    },
                    match input.get("write").and_then(Value::as_bool) {
                        Some(true) => ", read and write",
                        _ => ", read only",
                    }
                )
            }
            ("connections.setup", _) if text(input, "service").as_deref() == Some("github") => {
                "Start GitHub's sign-in for a new connection".into()
            }
            _ => return None,
        })
    }

    async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
        let started = Instant::now();
        let ms = || started.elapsed().as_millis() as u64;
        let Some(setups) = installed() else {
            return Ok(ToolResult::failure(
                "Connections aren't running on this hub.",
                ms(),
            ));
        };
        let Some(caller) = self.caller(&setups) else {
            return Ok(ToolResult::failure(
                "Connections are set up in the person's own conversation.",
                ms(),
            ));
        };
        let setup = text(&input, "setup");
        let need = || {
            setup
                .clone()
                .ok_or("Say which setup (`setup`).".to_string())
        };
        let done: Result<Value, String> = match self.name {
            "connections.list" => setups.listing(&caller).await,
            "connections.setup" => match text(&input, "service") {
                None => Err("Say which service (`service`).".into()),
                Some(service) => setups
                    .start(
                        &caller,
                        &service,
                        text(&input, "name").as_deref(),
                        text(&input, "for").as_deref(),
                    )
                    .await
                    .map(|view| {
                        let next = setups.next(&view);
                        json!({ "setup": view.id, "steps": view.steps, "next": next })
                    }),
            },
            "connections.step" => need().and_then(|id| {
                let state = match text(&input, "state").as_deref() {
                    Some("current") => StepState::Current,
                    Some("waiting") => StepState::Waiting,
                    Some("done") => StepState::Done,
                    _ => return Err("`state` is current, waiting or done".into()),
                };
                setups
                    .step(
                        &caller,
                        &id,
                        &text(&input, "step").unwrap_or_default(),
                        state,
                        text(&input, "note"),
                    )
                    .map(|v| json!({ "steps": v.steps }))
            }),
            "connections.keep" => need().and_then(|id| {
                setups
                    .keep(
                        &caller,
                        &id,
                        &text(&input, "field").unwrap_or_default(),
                        input
                            .get("value")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    )
                    .map(|v| json!({ "steps": v.steps }))
            }),
            "connections.test" => match need() {
                Err(e) => Err(e),
                Ok(id) => setups.test(&caller, &id).await.map(|v| {
                    json!({
                        "connected": v.connected.as_ref().map(|c| c.account.clone()),
                        "name": v.name,
                        "next": "Call connections.use to turn it on.",
                    })
                }),
            },
            "connections.use" => setups
                .use_it(
                    &caller,
                    setup.as_deref(),
                    text(&input, "service").as_deref(),
                    text(&input, "connection").as_deref(),
                    text(&input, "agent").as_deref(),
                    input.get("write").and_then(Value::as_bool).unwrap_or(false),
                    input
                        .get("default")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                )
                .await
                .map(|said| json!({ "done": said })),
            _ => Err("unknown".into()),
        };
        Ok(match done {
            Ok(v) => ToolResult::success(v, ms()),
            Err(e) => ToolResult::failure(e, ms()),
        })
    }
}

/// Registers `connections.*` for `service`.
pub fn register(service: &crate::serve::AgentService) {
    for tool in SetupTool::all(service.agent_id.as_str()) {
        if !service.tools.has(tool.name) {
            service.tools.register(Arc::new(tool));
        }
    }
}

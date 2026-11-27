//! An agent sets a connection up itself, on this computer: `connections.*`.
//!
//! Desk runs locally, so an agent can connect a service the way the person
//! would, and keep it where Connections keeps one. The person sees one card
//! for it in the conversation, which changes in place and says the one
//! thing to do now ([`Now`]); the agent and the hub do the rest:
//!
//! 1. `connections.setup {service, name?, for?, write?}` starts a setup, the
//!    person's one word for it below an unbound overseer: its key is kept,
//!    and it is turned on for whom it is for, once it works. The hub makes
//!    it, and from it alone come the vault tier, the agent, and its
//!    instance. Asked again in the conversation for the same connection, it
//!    is the same setup; another of the service there replaces it.
//! 2. How it gets its key ([`Way`]). GitHub: the GitHub CLI's sign-in on
//!    this computer, at the person's word on the card; else GitHub's device
//!    sign-in, a code the card shows, where Desk's GitHub app is set up;
//!    else, like every other service, a key the agent makes in the person's
//!    Chrome (through Desk's extension).
//! 3. In the browser: `connections.keep {setup, field, value}` keeps a field
//!    that isn't a secret (a site, an email). The key itself is read by
//!    `browser.capture_key` off the setup's key page ([`Setups::key_page`])
//!    and handed straight to [`Setups::keep_key`]: it never passes through
//!    the model. Signing in is the person's: the card asks them, and
//!    [`Setups::wait`] sees from the agent's tab when they have.
//! 4. Once every key is held, or GitHub's sign-in is approved, the hub
//!    tests it with the service (at the address kept for it; Metabase's
//!    only at the origin the key was read on), keeps it only on a pass, and
//!    turns it on ([`Setups::finish`]).
//! 5. `connections.wait {setup}` keeps the agent in its turn while the
//!    person does their part, and says what comes next. `connections.test`
//!    and `connections.use` test again and turn connections on by hand.
//!
//! `connections.list`, `connections.wait` and `connections.step` change
//! nothing.
//!
//! **Who may do what.** A setup is the person's (the one whose Desk this
//! is), the agent's that made it and the conversation's: no other agent,
//! person or session can use it, and a call without a conversation is
//! refused. An agent sets up a connection just for itself; one for all the
//! person's agents, or for another agent, and turning one on for another
//! agent, are the person's overseer's alone. Starting a setup, keeping a
//! field, testing and turning on are connection writes to Desk's gate:
//! freely for an unbound overseer, asking first below. What the person
//! chooses on the card (`POST /api/desk/connection-setups/{id}/{choice}`)
//! is theirs alone: no tool reaches it. The person's password is never read
//! or typed, and a token is only ever held as a [`Secret`], kept in the
//! vault, never logged or shown.
//!
//! The card the chat shows (`GET /api/desk/connection-setups?agent=`) is
//! the hub's record of the setup, never what the model wrote.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::surface::routing::{get, post};
use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::secret::Secret;
use workforce_core::computer::Effect;
use workforce_desk::connections::Level;
use workforce_skills::adapters::browser::keys as browser_keys;
pub use workforce_skills::adapters::browser::TabAt;

use super::catalog::{self, Connector};
use super::check::Found;
use super::{Conn, Connections, Refusal};

/// The longest one `connections.wait` waits: under an engine's tool limit.
const WAIT_MOST: Duration = Duration::from_secs(120);
/// How long a setup is kept.
const SETUP_TTL: Duration = Duration::from_secs(60 * 60);
/// The most setups one person keeps at once.
const MAX_SETUPS: usize = 16;
/// The tools, the ones that change nothing first.
pub const NAMES: [&str; 8] = [
    "connections.list",
    "connections.step",
    "connections.wait",
    "connections.stop",
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
    /// What to do on it, exactly: what the agent is told, and the card
    /// shows when the step is the person's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    /// The page it is done on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// How a setup gets its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Way {
    /// The GitHub CLI's sign-in on this computer, once the person says so.
    Cli,
    /// GitHub's device sign-in: the person approves a code.
    Device,
    /// A key the agent makes on the service's page in the browser.
    Browser,
    /// A token the person pasted on the card themselves.
    Token,
    /// The person's own app, made in the browser where none is kept, then
    /// approved on the service's sign-in page.
    App,
}

/// What a setup's card asks of the person, or tells them, now: the one
/// thing on it. The hub's words, never the agent's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NowKind {
    /// The agent is on it; nothing for the person to do.
    Working,
    /// A sign-in on this computer to use, at the person's word.
    Offer,
    /// A code to approve at the service.
    Code,
    /// The person signs in, in Desk's window.
    SignIn,
    /// The person approves Desk on the service's sign-in page.
    Approve,
    /// A step of the app's the agent couldn't do: the person does it.
    Step,
    Connected,
    Failed,
    /// Replaced by a newer setup of the same service, or cancelled.
    Ended,
}

/// The one thing a setup's card shows now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Now {
    pub kind: NowKind,
    pub title: String,
    pub detail: Option<String>,
    /// GitHub's code to approve, on the person's card alone.
    pub code: Option<String>,
    /// Where the code is approved.
    pub url: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    /// The card's one button: `use_cli`, `open_code`, `show_page`,
    /// `new_code`, `other_way`, `open_approve`, `open_step` or `retry`.
    pub action: Option<&'static str>,
    /// A quiet second choice beside it: `other_way`, `done_step`.
    pub other: Option<&'static str>,
    /// GitHub: the person may paste a token they make themselves, on this
    /// page (GitHub's new fine-grained token, filled in with the least it
    /// needs). The token goes from the card straight to the hub.
    pub paste: Option<String>,
    /// The person may paste their app's id and secret here: the step that
    /// keeps them is theirs.
    pub app_paste: bool,
}

/// GitHub's device sign-in, as the card shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Device {
    user_code: String,
    verification_uri: String,
    expires_at: DateTime<Utc>,
}

/// How a setup ended without connecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ended {
    /// A newer setup of the same service in the conversation took over.
    Replaced,
    /// The person or the agent stopped it, or the agent's turn ended while
    /// it was the agent's to carry on.
    Stopped,
}

/// Where the agent's tab is, for a setup waiting on the person to sign in.
#[async_trait]
pub trait Tabs: Send + Sync {
    /// Where `agent`'s tab for `person` is now, if it has one.
    async fn location(&self, agent: &str, person: &str) -> Option<TabAt>;
    /// Brings `agent`'s tab to the front of the person's Chrome.
    async fn show(&self, agent: &str) -> bool;
}

/// Desk's browser.
struct LiveTabs;

#[async_trait]
impl Tabs for LiveTabs {
    async fn location(&self, agent: &str, person: &str) -> Option<TabAt> {
        workforce_skills::adapters::browser::location(agent, person).await
    }
    async fn show(&self, agent: &str) -> bool {
        workforce_skills::adapters::browser::show(agent).await
    }
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
/// Jira's, Linear's, GitHub's, BankSync's and each Datadog region's at their origins,
/// and Metabase's, whose address is each person's own, on any `http` or `https`
/// origin. Nothing is registered later, so the registry never grows.
pub fn register_key_pages() {
    for service in ["jira", "linear", "github", "google", "freeagent", "bank"] {
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
        "google" => Recipe {
            page: |_| Some(GOOGLE_CONSOLE_CLIENTS.into()),
            origins: |_| vec![GOOGLE_CONSOLE.into()],
            paths: &["/auth/clients", "/apis/credentials"],
            workspace_segment: false,
            shaped: |field, v| app_shaped("google", field, v),
            shape: r"[0-9]{6,20}-[0-9a-z]{32}\.apps\.googleusercontent\.com|GOCSPX-[A-Za-z0-9_-]{20,40}",
            where_label: None,
            page_label: "Open Google Cloud",
            sign_in_label: "Sign in to Google",
            key_label: "Save the client ID and secret to the vault",
        },
        "freeagent" => Recipe {
            page: |_| Some(FREEAGENT_APPS.into()),
            origins: |_| vec![FREEAGENT_DEV.into()],
            paths: &["/apps"],
            workspace_segment: false,
            shaped: |field, v| app_shaped("freeagent", field, v),
            shape: r"[A-Za-z0-9_-]{16,64}",
            where_label: None,
            page_label: "Open FreeAgent's Developer Dashboard",
            sign_in_label: "Sign in to the FreeAgent Developer Dashboard",
            key_label: "Save the OAuth identifier and secret to the vault",
        },
        "bank" => Recipe {
            page: |_| Some(catalog::BANKSYNC_APP.into()),
            origins: |_| vec![catalog::BANKSYNC_APP.into()],
            // BankSync's settings sit under the workspace's own segment:
            // `/<workspace>/settings/developers/api-keys`.
            paths: &["/settings/developers"],
            workspace_segment: true,
            shaped: |_, v| v.starts_with("bsk_") && plain_token(v, 20),
            shape: r"bsk_[!#-&(-~]{16,4092}",
            where_label: Some("Say which bank"),
            page_label: "Open BankSync › Settings › Developers › API keys",
            sign_in_label: "Sign in to BankSync",
            key_label:
                "Make a key with banks:read and institutions:read only, and save it to the vault",
        },
        _ => return None,
    })
}

const GOOGLE_CONSOLE: &str = "https://console.cloud.google.com";
const GOOGLE_CONSOLE_CLIENTS: &str = "https://console.cloud.google.com/auth/clients";
const FREEAGENT_DEV: &str = "https://dev.freeagent.com";
const FREEAGENT_APPS: &str = "https://dev.freeagent.com/apps";

/// The keys a setup of the person's own app reads off its page.
const APP_FIELDS: [&str; 2] = ["client_id", "client_secret"];

/// Whether `v` is shaped like `field` of `service`'s app.
fn app_shaped(service: &str, field: &str, v: &str) -> bool {
    super::app::of(service).is_some_and(|app| app.field_shaped(field, v))
}

/// The keys a setup of `c` reads off its page: its app's, or its secret
/// fields.
fn secret_fields(c: &Connector) -> Vec<&'static str> {
    match c.sign_in {
        catalog::SignIn::App => APP_FIELDS.to_vec(),
        _ => c
            .fields
            .iter()
            .filter(|f| f.secret)
            .map(|f| f.name)
            .collect(),
    }
}

/// What a setup in the person's Chrome shows while Desk's extension there
/// is too old to use: reloading it, on Chrome's `page` for it, or its
/// updating itself.
fn outdated_now(outdated: crate::desk_chrome::Outdated, page: String, working: Now) -> Now {
    match outdated {
        crate::desk_chrome::Outdated::Reload => Now {
            kind: NowKind::Step,
            title: "Reload Desk's extension in Chrome".into(),
            detail: Some("It's an old build that can't update itself. On Chrome's page for it, press the reload arrow on Desk's card: from then on it keeps itself up to date, and the setup carries on.".into()),
            url: Some(page),
            action: Some("reload_extension"),
            ..working
        },
        crate::desk_chrome::Outdated::Updating => Now {
            detail: Some("Desk's extension in Chrome is updating itself: a moment.".into()),
            ..working
        },
    }
}

/// Whether `service`'s key goes to the site it was read on, so is only
/// read off that site's page: Metabase's, at each person's own address.
fn key_bound_to_page(service: &str) -> bool {
    service == "metabase"
}

/// Whether `value` is one secret marker, whole: a key handed over with
/// `cmd: secret`, never the key itself.
pub fn is_marker(value: &str) -> bool {
    let value = value.trim();
    matches!(
        workforce_common::secret_marker::parse_markers_spanned(value).as_slice(),
        [(span, _)] if *span == (0..value.len())
    )
}

/// The secret `marker` names, as the person the setup is for handed it over
/// in `caller`'s conversation (`cmd: secret`, their own message): resolved
/// here, so it never reaches the model. Anything else is refused: a value
/// read off a page or a tool's answer, context nobody typed (a repository's
/// code, other agents' lines), or another member's in a shared channel.
fn given_secret(caller: &Caller, marker: &str) -> Result<Secret<String>, String> {
    let marker = marker.trim();
    let id = match workforce_common::secret_marker::parse_markers(marker).as_slice() {
        [m] if is_marker(marker) => m.id.clone(),
        _ => return Err(GIVE_A_KEY.into()),
    };
    let (value, meta) = caller
        .session_key
        .as_deref()
        .and_then(|key| workforce_common::secret_marker::global_cache()?.lookup(key, &id))
        .ok_or_else(|| {
            format!("This conversation holds no such secret: it may have expired. {SEND_IT}")
        })?;
    match meta.source.handed_over_by() {
        Some(by) if by == caller.person => Ok(value),
        Some(_) => Err(format!(
            "Someone else in this conversation sent that secret, so it isn't kept as this person's key. {SEND_IT}"
        )),
        None => Err(format!(
            "That secret wasn't handed over by the person (it was read off a page, a tool's answer or other context), so it isn't kept as a key. {SEND_IT}"
        )),
    }
}

/// Which of `service`'s keys a secret the person named `name` is:
/// `<service>_key` the next still to hold, `<service>_<field>` that one.
/// `None` for no key of `service`'s.
fn key_named(service: &str, name: &str) -> Option<Option<&'static str>> {
    let rest = name.strip_prefix(service)?.strip_prefix('_')?;
    let c = catalog::find(service)?;
    match secret_fields(c).into_iter().find(|f| *f == rest) {
        Some(field) => Some(Some(field)),
        None => (rest == "key").then_some(None),
    }
}

/// What the agent is told of a setup whose key the person sent, once Desk
/// checked it: only Desk's own words and the account, never the service's.
fn sent_outcome(view: &SetupView) -> String {
    match &view.connected {
        Some(c) => format!(
            "{} is connected as {}: carry on with what they asked for, using it.",
            view.service_name, c.account
        ),
        None => format!(
            "{} refused the key: tell the person in a sentence; a new one is sent the same way.",
            view.service_name
        ),
    }
}

/// How the person hands their key over.
const SEND_IT: &str = "Ask the person to send it themselves with `cmd: secret <name> <key>`.";

/// How a key the person already has is handed over.
const GIVE_A_KEY: &str = "A key never passes through you. If the person has it already, mark the key step waiting (connections.step {setup, step: \"key\", state: \"waiting\"}): the card asks them to send it with `cmd: secret <service>_key <key>`, and the setup waits for it across turns: Desk keeps it, checks it and turns it on once it arrives, and tells you how that went. One sent already is kept with connections.keep, its [[secret:<name>]] marker as the value. Otherwise read it off the service's page with browser.capture_key.";

/// The step the person does: what to do and where, in the hub's words,
/// `<email>`, `<uri>` and `<name>` filled in when the card is shown.
fn app_step(id: &'static str, label: &str, url: Option<&str>, instruction: &str) -> Step {
    Step {
        instruction: Some(instruction.to_string()),
        url: url.map(str::to_string),
        ..step(id, label)
    }
}

/// The steps of a setup of the person's own app, from where it is.
fn app_steps(service: &str, app: super::app::AppState, whose: &str) -> Vec<Step> {
    use super::app::AppState;
    let c = catalog::find(service).expect("an app's service is in the catalog");
    let approve = step(
        "approve",
        &match service {
            "freeagent" => "Approve Desk on FreeAgent as <name>".to_string(),
            _ => format!("Approve Desk on {}", c.name),
        },
    );
    let check = step(
        "check",
        &format!("Check it works, then turn it on for {whose}"),
    );
    let page = |url: &str| {
        app_step(
            "page",
            recipe(service).map_or("Open the page", |r| r.page_label),
            Some(url),
            &format!("Open {url} with browser.navigate."),
        )
    };
    let sign_in = || {
        app_step(
            "sign_in",
            recipe(service).map_or("Sign in", |r| r.sign_in_label),
            None,
            match service {
                "freeagent" => "Sign in to FreeAgent's Developer Dashboard in this window, or sign up there for free. Desk never reads or types your password.",
                _ => "Sign in to Google in this window. Desk never reads or types your password.",
            },
        )
    };
    let key = || {
        app_step(
            "key",
            recipe(service).map_or("Save the app's keys", |r| r.key_label),
            None,
            &match service {
                "freeagent" => "On the app's page, read the OAuth identifier with browser.capture_key {connector: \"freeagent\", field: \"client_id\", selector: <the element showing it>}, then the OAuth secret with field: \"client_secret\" and its own element's selector. The two look alike, so Desk can't tell them apart on its own: give each one's selector from browser.snapshot. They never come back to you.".to_string(),
                _ => "On the dialog that shows the new client, read the Client ID with browser.capture_key {connector: \"google\", field: \"client_id\"}, then the Client secret with field: \"client_secret\", each with no selector: Desk finds them, and they never come back to you.".to_string(),
            },
        )
    };
    let mut steps = match (service, app) {
        (_, AppState::Ready) => vec![approve, check],
        ("freeagent", AppState::Moved) => vec![
            page(FREEAGENT_APPS),
            sign_in(),
            app_step(
                "address",
                "Add Desk's new address to your FreeAgent app",
                Some(FREEAGENT_APPS),
                "Open your Desk app on https://dev.freeagent.com/apps and click Edit. Add <uri> to its OAuth redirect URIs, keeping the ones there, then Save. Then call connections.keep {field: \"redirect_uri_added\", value: \"yes\"}.",
            ),
            approve,
            check,
        ],
        ("freeagent", _) => vec![
            page(FREEAGENT_APPS),
            sign_in(),
            app_step(
                "make",
                "Make Desk's FreeAgent app",
                Some(FREEAGENT_APPS),
                "Click Create New App. Name it \"Desk\", description \"Desk on this computer\", OAuth redirect URIs <uri>. For who may use the app, choose all FreeAgent users, so each business can approve it, and leave accountancy practices off. Then Create.",
            ),
            key(),
            approve,
            check,
        ],
        _ => vec![
            page("https://console.cloud.google.com/projectcreate"),
            sign_in(),
            app_step(
                "project",
                "Make a Google Cloud project called Desk",
                Some("https://console.cloud.google.com/projectcreate"),
                "Name the project \"Desk\" and click Create. If a project called Desk is there already, pick it in the project picker instead. On a work (Google Workspace) account, leave Location set to the organisation.",
            ),
            app_step(
                "apis",
                "Turn on Calendar, Drive, Gmail, Sheets and Docs",
                Some("https://console.cloud.google.com/apis/library/calendar-json.googleapis.com"),
                "In the Desk project, open each of these and click Enable: https://console.cloud.google.com/apis/library/calendar-json.googleapis.com, https://console.cloud.google.com/apis/library/drive.googleapis.com, https://console.cloud.google.com/apis/library/gmail.googleapis.com, https://console.cloud.google.com/apis/library/sheets.googleapis.com and https://console.cloud.google.com/apis/library/docs.googleapis.com.",
            ),
            app_step(
                "branding",
                "Set up Desk's sign-in screen",
                Some("https://console.cloud.google.com/auth/branding"),
                "Open https://console.cloud.google.com/auth/branding and click Get started. App name \"Desk\", user support email <email>, then Next. Audience: Internal if it is offered (a work account), else External, then Next. Contact email <email>, then Next. Agree, then Create.",
            ),
            app_step(
                "audience",
                "Publish the app, so it keeps working",
                Some("https://console.cloud.google.com/auth/audience"),
                "If the audience is Internal, mark this step done. If it is External: first tell the person, in one line, that this keeps their own app working beyond 7 days, and that Google will call it unverified because only they use it. Then open https://console.cloud.google.com/auth/audience, click Publish app, then Confirm, so the status reads In production.",
            ),
            app_step(
                "access",
                "Add what Desk asks for",
                Some("https://console.cloud.google.com/auth/scopes"),
                "Open https://console.cloud.google.com/auth/scopes and click Add or remove scopes. Tick https://www.googleapis.com/auth/calendar, https://www.googleapis.com/auth/drive, https://mail.google.com/, https://www.googleapis.com/auth/userinfo.email and openid, click Update, then Save.",
            ),
            app_step(
                "client",
                "Make Desk's sign-in client",
                Some("https://console.cloud.google.com/auth/clients/create"),
                "Open https://console.cloud.google.com/auth/clients/create. Application type \"Desktop app\", name \"Desk\", then Create.",
            ),
            key(),
            approve,
            check,
        ],
    };
    steps[0].state = StepState::Current;
    steps
}

/// Why a service isn't one an agent sets up, and what the person does.
fn by_hand(service: &str) -> &'static str {
    match service {
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
    /// The conversation's key: where the person sends its key.
    conversation: Option<String>,
    service: &'static str,
    name: String,
    key: String,
    level: Level,
    /// The agent it is to be turned on for.
    for_agent: Option<String>,
    steps: Vec<Step>,
    fields: BTreeMap<String, String>,
    secrets: BTreeMap<String, Secret<String>>,
    /// How it gets its key.
    way: Way,
    /// GitHub's device sign-in, where it signs in that way.
    flow: Option<String>,
    /// Where that sign-in is, as last seen.
    flow_state: Option<super::FlowState>,
    device: Option<Device>,
    /// The GitHub CLI's sign-in it offers: whose, and its scopes.
    offer: Option<super::gh::Login>,
    /// Whether the agent it is for takes writes with it.
    write: bool,
    /// Whether the agent running it is the person's overseer.
    overseer: bool,
    /// Whom it is for, in the person's words.
    whose: String,
    /// Its check and turning on are under way, or done.
    finishing: bool,
    /// The person said on its card they're signed in, where Desk didn't
    /// see it.
    said_signed_in: bool,
    /// The person did a step of the app's on its card.
    said_done: Option<&'static str>,
    /// The service's sign-in page the person approves Desk on.
    approve_url: Option<String>,
    /// This hub's address the person's app redirects to.
    redirect: Option<String>,
    ended: Option<Ended>,
    /// When the agent last asked for it: its start, or its asking again.
    asked_at: DateTime<Utc>,
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
    /// When the agent last asked for it: the message that did shows its
    /// card.
    pub asked_at: DateTime<Utc>,
    pub connected: Option<Connected>,
    pub way: Way,
    /// The one thing its card shows now.
    pub now: Now,
    pub ended: Option<Ended>,
    pub write: bool,
    /// Whom it is for, in the person's words.
    pub for_whom: String,
}

impl Setup {
    /// The one thing its card shows now, from its own record.
    fn now(&self, service: &str) -> Now {
        let mut now = self.now_of(service);
        // A setup in the person's Chrome waits on an extension too old to
        // trust: Desk opens Chrome's page for it, the person reloads it.
        if matches!(self.way, Way::Browser | Way::App) && now.kind == NowKind::Working {
            if let Some((outdated, page)) =
                crate::desk_chrome::installed().and_then(|c| c.outdated())
            {
                now = outdated_now(outdated, page, now);
            }
        }
        // GitHub: a token of the person's own, beside whatever else the
        // card asks, until it is connected.
        if self.service == "github"
            && self.connected.is_none()
            && !self.finishing
            && !matches!(now.kind, NowKind::Ended | NowKind::Connected)
        {
            now.paste = Some(github_token_page(&self.name, self.write));
        }
        now
    }

    fn now_of(&self, service: &str) -> Now {
        let now = |kind, title: String| Now {
            kind,
            title,
            detail: None,
            code: None,
            url: None,
            expires_at: None,
            action: None,
            other: None,
            paste: None,
            app_paste: false,
        };
        let access = match self.write {
            true => "read and write",
            false => "read only",
        };
        if let Some(ended) = self.ended {
            return now(
                NowKind::Ended,
                match ended {
                    Ended::Replaced => format!("This {service} setup was replaced"),
                    Ended::Stopped => format!("This {service} setup was stopped"),
                },
            );
        }
        let failed = self.steps.iter().find(|s| s.state == StepState::Failed);
        if let Some(c) = self
            .connected
            .as_ref()
            .filter(|c| c.on_for.is_some() || failed.is_none())
        {
            return match &c.on_for {
                Some(on_for) => Now {
                    detail: Some(format!(
                        "{on_for} uses it · {} · kept in the vault on this computer",
                        match c.write {
                            Some(true) => "read and write",
                            _ => "read only",
                        }
                    )),
                    ..now(
                        NowKind::Connected,
                        format!("{service} connected as {}", c.account),
                    )
                },
                None => now(
                    NowKind::Working,
                    format!("Turning {service} on for {}", self.whose),
                ),
            };
        }
        if let Some(failed) = failed {
            let said = failed
                .note
                .clone()
                .unwrap_or_else(|| "It didn't work.".into());
            if self.connected.is_some() {
                // Kept, but not turned on.
                return Now {
                    detail: Some(format!(
                        "{said} It's kept: turn it on for {} in Connections.",
                        self.whose
                    )),
                    action: Some("open_connections"),
                    ..now(NowKind::Failed, format!("{service} isn't on yet"))
                };
            }
            let action = match self.way {
                Way::Device => "new_code",
                Way::Cli => "other_way",
                Way::Browser | Way::App => "retry",
                Way::Token => "paste_token",
            };
            return Now {
                detail: Some(format!("{said} Nothing was kept.")),
                action: Some(action),
                ..now(NowKind::Failed, format!("{service} didn't connect"))
            };
        }
        match self.way {
            Way::Cli if !self.finishing => {
                let offer = self.offer.as_ref();
                let scopes = offer
                    .map(|o| o.scopes.join(", "))
                    .filter(|s| !s.is_empty())
                    .map_or_else(|| "the same access".to_string(), |s| format!("the same access ({s})"));
                Now {
                    detail: Some(format!(
                        "Through GitHub CLI. {} gets {scopes}, {access}. Desk keeps a copy in the vault on this computer.",
                        self.whose
                    )),
                    action: Some("use_cli"),
                    other: Some("other_way"),
                    ..now(
                        NowKind::Offer,
                        format!(
                            "{service} is signed in on this computer as {}",
                            offer.map_or("you", |o| o.account.as_str())
                        ),
                    )
                }
            }
            Way::Device => match (&self.device, &self.flow_state) {
                (Some(d), None | Some(super::FlowState::Waiting)) => Now {
                    detail: Some(
                        "Desk carries on as soon as you approve it on GitHub.".into(),
                    ),
                    code: Some(d.user_code.clone()),
                    url: Some(d.verification_uri.clone()),
                    expires_at: Some(d.expires_at),
                    action: Some("open_code"),
                    ..now(NowKind::Code, format!("Enter this code on {service}"))
                },
                _ => now(
                    NowKind::Working,
                    format!("Checking {service} and turning it on for {}", self.whose),
                ),
            },
            Way::App if !self.finishing => {
                let steps = self.filled_steps();
                let waiting = steps
                    .iter()
                    .enumerate()
                    .find(|(_, x)| x.state == StepState::Waiting);
                match waiting {
                    Some((_, x)) if x.id == "approve" => Now {
                        detail: Some(match (service, self.named()) {
                            ("Google", _) => "It's your own app, so Google may say it hasn't verified it: click Advanced, then Go to Desk. Desk carries on as soon as you allow it.".to_string(),
                            (_, Some(name)) => format!("On {service}'s page, sign in as {name}, then approve. Desk carries on as soon as you do."),
                            _ => "Desk carries on as soon as you approve.".to_string(),
                        }),
                        url: self.approve_url.clone(),
                        action: Some("open_approve"),
                        ..now(NowKind::Approve, format!("Approve Desk on {service}"))
                    },
                    Some((_, x)) if x.id == "sign_in" => Now {
                        detail: x.instruction.clone(),
                        action: Some("show_page"),
                        ..now(NowKind::SignIn, x.label.clone())
                    },
                    Some((n, x)) => Now {
                        detail: x.instruction.clone(),
                        url: x.url.clone(),
                        action: Some("open_step"),
                        other: Some("done_step"),
                        app_paste: x.id == "key",
                        ..now(
                            NowKind::Step,
                            format!("Step {} of {}: {}", n + 1, steps.len(), x.label),
                        )
                    },
                    None => Now {
                        detail: steps
                            .iter()
                            .find(|x| x.state != StepState::Done)
                            .map(|x| x.label.clone()),
                        ..now(
                            NowKind::Working,
                            format!("Setting up {service} for {}", self.whose),
                        )
                    },
                }
            }
            Way::App => now(
                NowKind::Working,
                format!("Checking {service} and turning it on for {}", self.whose),
            ),
            _ => match self.steps.iter().find(|s| s.state == StepState::Waiting) {
                // The person hands over a key they made themselves: the
                // setup waits on them, across turns.
                Some(x) if x.id == "key" => Now {
                    detail: Some(format!(
                        "Send it here with `cmd: secret {}_key <the key>`: it goes straight to the vault on this computer, never into the chat, and Desk checks it and turns it on.",
                        self.service
                    )),
                    ..now(NowKind::Step, format!("Send {service}'s key in this conversation"))
                },
                Some(_) => Now {
                    detail: Some("Your password stays with you: Desk never reads or types it. It carries on by itself once you're in.".into()),
                    action: Some("show_page"),
                    ..now(NowKind::SignIn, format!("Sign in to {service} in Desk's window"))
                },
                None => Now {
                    detail: self
                        .steps
                        .iter()
                        .find(|s| s.state != StepState::Done)
                        .map(|s| s.label.clone()),
                    ..now(
                        NowKind::Working,
                        format!("Setting up {service} for {}", self.whose),
                    )
                },
            },
        }
    }

    /// Ends it, `why`: nothing it holds is kept, and a sign-in under way
    /// is let go.
    fn end(&mut self, why: Ended, connections: &Connections) {
        self.ended = Some(why);
        self.secrets.clear();
        if let Some(flow) = &self.flow {
            connections.cancel(&self.person, flow);
        }
    }

    /// Whether it waits on its agent to carry it on: nothing of the
    /// person's, Desk's or the service's is under way.
    fn agents_to_do(&self) -> bool {
        self.ended.is_none()
            && self.connected.is_none()
            && !self.finishing
            && self.now(self.service).kind == NowKind::Working
    }

    /// The name the person gave it, where it isn't the service's own.
    fn named(&self) -> Option<&str> {
        let c = catalog::find(self.service)?;
        (self.name != c.name).then_some(self.name.as_str())
    }

    /// Its steps, their words filled in: the account's email, this hub's
    /// address and the connection's name.
    fn filled_steps(&self) -> Vec<Step> {
        let fill = |text: &str| {
            text.replace(
                "<email>",
                self.fields
                    .get("email")
                    .map_or("your email address", String::as_str),
            )
            .replace(
                "<uri>",
                self.redirect.as_deref().unwrap_or("Desk's address"),
            )
            .replace("<name>", self.named().unwrap_or("your business"))
        };
        self.steps
            .iter()
            .map(|x| Step {
                label: fill(&x.label),
                instruction: x.instruction.as_deref().map(fill),
                ..x.clone()
            })
            .collect()
    }

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
            steps: self.filled_steps(),
            started_at: self.started_at,
            asked_at: self.asked_at,
            connected: self.connected.clone(),
            way: self.way,
            now: self.now(c.name),
            ended: self.ended,
            write: self.write,
            for_whom: self.whose.clone(),
        }
    }

    /// Takes in where its device sign-in is: a code turned down, expired or
    /// failed is its check failed, in plain words.
    fn sync(&mut self, connections: &Connections) {
        let Some(flow) = &self.flow else { return };
        let state = connections.flow(&self.person, flow);
        let service = catalog::find(self.service).map_or(self.service, |c| c.name);
        if self.connected.is_none() {
            let said = match &state {
                Some(super::FlowState::Expired) => Some(match self.way {
                    Way::App => format!("{service}'s page wasn't approved in time."),
                    _ => "The code expired before it was approved.".to_string(),
                }),
                Some(super::FlowState::Cancelled) => {
                    Some(format!("The sign-in was turned down on {service}."))
                }
                Some(super::FlowState::Failed { error }) => {
                    Some(format!("{}.", error.trim_end_matches('.')))
                }
                None => Some("The sign-in ended.".to_string()),
                _ => None,
            };
            if let Some(said) = said {
                if !self.steps.iter().any(|s| s.state == StepState::Failed) {
                    self.mark("check", StepState::Failed, Some(said));
                }
            }
        }
        self.flow_state = state;
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
    /// `kept`: every key is held, and Desk checks it and turns it on
    /// itself; or `still needs <field>`.
    pub test: String,
    /// Every key is held: [`Setups::finish`] is next.
    #[serde(skip)]
    pub ready: bool,
    /// The setup it was kept for.
    #[serde(skip)]
    pub setup: String,
}

/// What the gate's question says of what the hub couldn't resolve.
const UNKNOWN: &str = "unknown";

/// The hub's facts about callers.
pub trait Callers: Send + Sync {
    /// Whether `holder` is the one whose Desk this is.
    fn holds_the_hub(&self, holder: crate::authz::Holder<'_>) -> bool;
    /// Whether `agent` is the person's overseer.
    fn oversees(&self, agent: &str) -> bool;
    /// The agent `name` (its name or id) names, if there is one.
    fn agent_named(&self, name: &str) -> Option<String>;
    /// Wakes `caller`'s agent in its conversation with `note`: what Desk
    /// did there without it, for it to carry on from.
    fn tell(&self, caller: &Caller, note: String);
}

/// The setups under way, over Connections.
pub struct Setups {
    pub connections: Arc<Connections>,
    pub callers: Arc<dyn Callers>,
    tabs: Arc<dyn Tabs>,
    setups: Mutex<HashMap<String, Setup>>,
}

/// The longest a field kept by `keep` is.
const MAX_FIELD: usize = 2048;

impl Setups {
    pub fn new(connections: Arc<Connections>, callers: Arc<dyn Callers>) -> Self {
        Self {
            connections,
            callers,
            tabs: Arc::new(LiveTabs),
            setups: Mutex::new(HashMap::new()),
        }
    }

    /// These setups, over `tabs` instead of Desk's browser: a test's.
    pub fn with_tabs(mut self, tabs: Arc<dyn Tabs>) -> Self {
        self.tabs = tabs;
        self
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
        setup.sync(&self.connections);
        f(setup)
    }

    fn check_caller(&self, caller: &Caller) -> Result<(), String> {
        if caller.session.is_none() {
            return Err("Connections are set up in a conversation with the person.".into());
        }
        match self
            .callers
            .holds_the_hub(crate::authz::Holder::full(&caller.person))
        {
            true => Ok(()),
            false => Err(
                "Only the person whose Desk this is sets up its connections, in their own conversation.".into(),
            ),
        }
    }

    /// Starts a setup of `service`, named `name`, for `whom` (`me`, `all`,
    /// or an agent), that turns it on for them with writes or not. The
    /// same setup again, while it runs in this conversation, is the one
    /// already under way: its card stays the one card.
    pub async fn start(
        self: &Arc<Self>,
        caller: &Caller,
        service: &str,
        name: Option<&str>,
        whom: Option<&str>,
        write: bool,
    ) -> Result<SetupView, String> {
        self.check_caller(caller)?;
        let c = catalog::find(service)
            .ok_or_else(|| format!("Desk has no connector {service}. It sets up: Jira, Linear, Datadog, Metabase, GitHub, Google, FreeAgent and Bank."))?;
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
        // The one under way here already, for the same connection: the
        // same card. Another of the service here is replaced by this one.
        if let Some(view) = self.again(caller, c.id, &name, &for_agent, write) {
            return Ok(view);
        }
        // Room for it, before anything starts (GitHub's sign-in).
        let full = || {
            let mut setups = self.setups.lock().expect("setups poisoned");
            let now = Instant::now();
            setups.retain(|_, s| s.until > now);
            setups
                .values()
                .filter(|s| {
                    s.person == caller.person
                        && s.ended.is_none()
                        && !s.steps.iter().any(|x| x.state == StepState::Failed)
                })
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
        // GitHub: the CLI's sign-in on this computer where there is one,
        // else GitHub's own sign-in where Desk's GitHub app is set up, else
        // a token made in the browser.
        let offer = match c.id {
            "github" => self.connections.services.gh.login().await,
            _ => None,
        };
        let app = self.connections.app_state(c);
        let way = match (c.id, &offer, app) {
            (_, _, Some(_)) => Way::App,
            ("github", Some(_), _) => Way::Cli,
            ("github", None, _) if self.connections.github_client_id.is_some() => Way::Device,
            _ => Way::Browser,
        };
        let steps = match app {
            Some(app) => app_steps(c.id, app, &whose),
            None => steps_for(way, &recipe, &whose, offer.as_ref()),
        };
        let id = uuid::Uuid::new_v4().simple().to_string();
        let setup = Setup {
            id: id.clone(),
            person: caller.person.clone(),
            by: caller.agent.clone(),
            session: caller.session.clone(),
            conversation: caller.session_key.clone(),
            service: c.id,
            name,
            key,
            level,
            for_agent,
            steps,
            fields: BTreeMap::new(),
            secrets: BTreeMap::new(),
            way,
            flow: None,
            flow_state: None,
            device: None,
            offer,
            write,
            overseer: caller.overseer,
            whose,
            finishing: false,
            said_signed_in: false,
            said_done: None,
            approve_url: None,
            redirect: super::app::of(c.id).map(|a| a.redirect(&self.connections.gateway)),
            ended: None,
            asked_at: Utc::now(),
            read_at: None,
            started_at: Utc::now(),
            until: Instant::now() + SETUP_TTL,
            connected: None,
        };
        {
            let mut setups = self.setups.lock().expect("setups poisoned");
            let now = Instant::now();
            setups.retain(|_, s| s.until > now);
            if setups
                .values()
                .filter(|s| {
                    s.person == caller.person
                        && s.ended.is_none()
                        && !s.steps.iter().any(|x| x.state == StepState::Failed)
                })
                .count()
                >= MAX_SETUPS
            {
                return Err("Too many setups are under way. Finish one first.".into());
            }
            setups.insert(id.clone(), setup);
        }
        if way == Way::Device {
            if let Err(e) = self.begin_device(caller, &id).await {
                self.setups.lock().expect("setups poisoned").remove(&id);
                return Err(e);
            }
        }
        // The person's app is kept already: straight to approving it.
        if way == Way::App {
            if let Err(e) = self.advance(caller, &id).await {
                self.setups.lock().expect("setups poisoned").remove(&id);
                return Err(e);
            }
        }
        self.with(caller, &id, |s| Ok(s.view()))
    }

    /// The setup of `service` under way in `caller`'s conversation, when
    /// it is for the same connection; any other of it there is replaced.
    fn again(
        &self,
        caller: &Caller,
        service: &str,
        name: &str,
        for_agent: &Option<String>,
        write: bool,
    ) -> Option<SetupView> {
        let mut setups = self.setups.lock().expect("setups poisoned");
        let now = Instant::now();
        setups.retain(|_, s| s.until > now);
        let mut same = None;
        for s in setups.values_mut().filter(|s| {
            s.person == caller.person
                && s.by == caller.agent
                && s.session == caller.session
                && s.service == service
                && s.ended.is_none()
        }) {
            s.sync(&self.connections);
            // Kept already, on or not: a new setup would keep it twice.
            if s.connected.is_some() {
                continue;
            }
            let failed = s.steps.iter().any(|x| x.state == StepState::Failed);
            let alike = s.name == name && &s.for_agent == for_agent && s.write == write;
            // One Desk is checking and turning on is left to finish.
            if s.finishing && !failed {
                if same.is_none() && alike {
                    same = Some(s.view());
                }
                continue;
            }
            // A failed one is replaced too, so its card goes.
            if same.is_none() && alike && !failed {
                s.asked_at = Utc::now();
                same = Some(s.view());
                continue;
            }
            s.end(Ended::Replaced, &self.connections);
        }
        same
    }

    /// Starts GitHub's device sign-in for setup `id`, a new code each
    /// time, and watches it: once the person approves it, the hub checks
    /// the connection and turns it on itself.
    async fn begin_device(self: &Arc<Self>, caller: &Caller, id: &str) -> Result<(), String> {
        let (level, key, name, old) = self.with(caller, id, |s| {
            Ok((
                s.level.clone(),
                s.key.clone(),
                s.name.clone(),
                s.flow.clone(),
            ))
        })?;
        if let Some(old) = old {
            self.connections.cancel(&caller.person, &old);
        }
        let c = catalog::find("github").expect("in the catalog");
        let started = match self
            .connections
            .begin_github(&caller.person, c, level, Some(key), Some(name))
            .await
        {
            Ok(started) => started,
            Err(e) => {
                let e = refused(e);
                let note: String = e.chars().take(200).collect();
                self.with(caller, id, |s| {
                    s.flow = None;
                    s.flow_state = None;
                    s.device = None;
                    s.mark("check", StepState::Failed, Some(note));
                    Ok(())
                })?;
                return Err(e);
            }
        };
        self.with(caller, id, |s| {
            s.flow = Some(started.flow.clone());
            s.flow_state = Some(super::FlowState::Waiting);
            s.device = Some(Device {
                user_code: started.user_code.clone().unwrap_or_default(),
                verification_uri: started
                    .verification_uri
                    .clone()
                    .unwrap_or_else(|| "https://github.com/login/device".into()),
                expires_at: Utc::now()
                    + chrono::Duration::seconds(started.expires_in.min(3600) as i64),
            });
            for step in s.steps.iter_mut() {
                step.state = StepState::Todo;
                step.note = None;
            }
            s.steps[0].state = StepState::Waiting;
            Ok(())
        })?;
        self.watch_flow(caller, id, started.flow);
        Ok(())
    }

    /// Watches sign-in `flow` of setup `id`: once the person approves it,
    /// the hub checks the connection and turns it on itself.
    fn watch_flow(self: &Arc<Self>, caller: &Caller, id: &str, flow: String) {
        let this = Arc::clone(self);
        let caller = caller.clone();
        let id = id.to_string();
        let every = self
            .connections
            .services
            .poll_floor
            .min(Duration::from_secs(1));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let state = this.with(&caller, &id, |s| {
                    Ok((s.flow.clone(), s.flow_state.clone(), s.ended))
                });
                match state {
                    // A newer code, or a newer setup, took over.
                    Ok((Some(f), _, None)) if f != flow => return,
                    Ok((_, Some(super::FlowState::Waiting), None)) => continue,
                    Ok((_, Some(super::FlowState::Connected { .. }), None)) => {
                        this.finish(&caller, &id).await;
                        return;
                    }
                    _ => return,
                }
            }
        });
    }

    /// Moves setup `id` of the person's own app on, once every step before
    /// approving is done: the app's id and secret, where they are held,
    /// kept as the person's app, then the service's sign-in page opened
    /// for the person to approve.
    async fn advance(self: &Arc<Self>, caller: &Caller, id: &str) -> Result<(), String> {
        let ready = self.with(caller, id, |s| {
            Ok(s.way == Way::App
                && s.flow.is_none()
                && s.connected.is_none()
                && s.steps.iter().any(|x| x.id == "approve")
                && s.steps
                    .iter()
                    .take_while(|x| x.id != "approve")
                    .all(|x| x.state == StepState::Done))
        })?;
        if !ready {
            return Ok(());
        }
        let (service, app) = self.with(caller, id, |s| {
            let app = match (s.secrets.get("client_id"), s.secrets.get("client_secret")) {
                (Some(id), Some(secret)) => {
                    Some((id.expose_secret().clone(), secret.expose_secret().clone()))
                }
                _ => None,
            };
            Ok((s.service, app))
        })?;
        let fail = |e: String| {
            let note: String = e.chars().take(200).collect();
            let _ = self.with(caller, id, |s| {
                s.secrets.clear();
                s.mark("check", StepState::Failed, Some(note));
                Ok(())
            });
            e
        };
        if let Some((client_id, client_secret)) = app {
            self.connections
                .keep_app(&caller.person, service, &client_id, &client_secret)
                .await
                .map_err(|r| fail(refused(r)))?;
            self.with(caller, id, |s| {
                s.secrets.clear();
                Ok(())
            })?;
        }
        let (level, key, name) = self.with(caller, id, |s| {
            Ok((
                s.level.clone(),
                s.key.clone(),
                s.named().map(str::to_string),
            ))
        })?;
        let c = catalog::find(service).expect("in the catalog");
        let started = self
            .connections
            .begin_app(&caller.person, c, level, Some(key), name, false)
            .map_err(|r| fail(refused(r)))?;
        self.with(caller, id, |s| {
            s.flow = Some(started.flow.clone());
            s.flow_state = Some(super::FlowState::Waiting);
            s.approve_url = started.url.clone();
            s.mark("approve", StepState::Waiting, None);
            Ok(())
        })?;
        self.watch_flow(caller, id, started.flow);
        Ok(())
    }

    /// Checks setup `id`'s connection, keeps it on a pass and turns it on
    /// for whom it is for: what the person's word on the card, or the key
    /// read off the page, starts. Once.
    pub async fn finish(self: &Arc<Self>, caller: &Caller, id: &str) {
        // The person's app's keys held: approving it is next.
        if self
            .with(caller, id, |s| Ok(s.way == Way::App && s.flow.is_none()))
            .unwrap_or(false)
        {
            let _ = self.advance(caller, id).await;
            return;
        }
        if let Some(write) = self.claim(caller, id) {
            self.finish_claimed(caller, id, write).await;
        }
    }

    /// Takes setup `id`'s check and turning on as begun, so nothing else
    /// begins it and a turn's end doesn't stop it: whether it takes writes,
    /// or `None` where it was begun already.
    fn claim(&self, caller: &Caller, id: &str) -> Option<bool> {
        self.with(caller, id, |s| {
            Ok((!std::mem::replace(&mut s.finishing, true)).then_some(s.write))
        })
        .ok()
        .flatten()
    }

    /// [`Self::finish`], once [`Self::claim`]ed.
    async fn finish_claimed(self: &Arc<Self>, caller: &Caller, id: &str, write: bool) {
        if let Err(e) = self.test(caller, id).await {
            let _ = self.with(caller, id, |s| {
                s.finishing = false;
                if !s.steps.iter().any(|x| x.state == StepState::Failed) {
                    let note: String = e.chars().take(200).collect();
                    s.mark("check", StepState::Failed, Some(note));
                }
                Ok(())
            });
            return;
        }
        if let Err(e) = self
            .use_it(caller, Some(id), None, None, None, write, false)
            .await
        {
            let note: String = e.chars().take(200).collect();
            let _ = self.with(caller, id, |s| {
                s.finishing = false;
                s.mark("check", StepState::Failed, Some(note));
                Ok(())
            });
        }
    }

    /// The caller setup `id` was started by, for what the person does on
    /// its card: `None` for no such setup of theirs.
    fn caller_of(&self, person: &str, id: &str) -> Option<Caller> {
        let setups = self.setups.lock().expect("setups poisoned");
        let s = setups
            .get(id)
            .filter(|s| s.person == person && s.until > Instant::now() && s.ended.is_none())?;
        Some(Caller {
            person: s.person.clone(),
            agent: s.by.clone(),
            session: s.session.clone(),
            session_key: None,
            overseer: s.overseer,
        })
    }

    /// The person said, on the card, to use the GitHub CLI's sign-in: its
    /// token is read now, checked, kept and turned on.
    pub async fn use_cli(self: &Arc<Self>, person: &str, id: &str) -> Result<SetupView, String> {
        let caller = self.caller_of(person, id).ok_or(GONE)?;
        let offered = self.with(&caller, id, |s| {
            Ok(s.way == Way::Cli && s.connected.is_none() && !s.finishing)
        })?;
        if !offered {
            return self.with(&caller, id, |s| Ok(s.view()));
        }
        let token = self.connections.services.gh.token().await;
        let kept = self.with(&caller, id, |s| match token {
            Some(token) => {
                s.secrets.insert("token".into(), token);
                for step in ["use", "sign_in", "page", "key"] {
                    s.mark(step, StepState::Done, None);
                }
                Ok(true)
            }
            None => {
                s.mark(
                    "check",
                    StepState::Failed,
                    Some("GitHub CLI didn't give Desk its sign-in.".into()),
                );
                Ok(false)
            }
        })?;
        if kept {
            self.finish(&caller, id).await;
        }
        self.with(&caller, id, |s| Ok(s.view()))
    }

    /// The person wants to sign in another way than the one offered: with
    /// GitHub's own sign-in where Desk has it, else a token made in the
    /// browser, which the agent goes on to make.
    pub async fn other_way(self: &Arc<Self>, person: &str, id: &str) -> Result<SetupView, String> {
        let caller = self.caller_of(person, id).ok_or(GONE)?;
        let way = self.with(&caller, id, |s| {
            if s.connected.is_some() || s.finishing || matches!(s.way, Way::Browser | Way::App) {
                return Ok(None);
            }
            let way = match (s.way, self.connections.github_client_id.is_some()) {
                (Way::Cli, true) => Way::Device,
                _ => Way::Browser,
            };
            let recipe = recipe(s.service).expect("a setup's service has a recipe");
            s.way = way;
            s.offer = None;
            s.secrets.clear();
            s.steps = steps_for(way, &recipe, &s.whose, None);
            Ok(Some(way))
        })?;
        if way == Some(Way::Device) {
            self.begin_device(&caller, id).await?;
        }
        self.with(&caller, id, |s| Ok(s.view()))
    }

    /// The person asked for a new code, on the card of a device sign-in
    /// that didn't connect.
    pub async fn new_code(self: &Arc<Self>, person: &str, id: &str) -> Result<SetupView, String> {
        let caller = self.caller_of(person, id).ok_or(GONE)?;
        let again = self.with(&caller, id, |s| {
            Ok(s.way == Way::Device && s.connected.is_none() && !s.finishing)
        })?;
        if again {
            self.begin_device(&caller, id).await?;
        }
        self.with(&caller, id, |s| Ok(s.view()))
    }

    /// The person pasted a GitHub token of their own on setup `id`'s card:
    /// it goes no further than this, the vault and GitHub. Any other way
    /// under way stops; the token is checked, kept and turned on.
    pub async fn paste_token(
        self: &Arc<Self>,
        person: &str,
        id: &str,
        token: Secret<String>,
    ) -> Result<SetupView, String> {
        let caller = self.caller_of(person, id).ok_or(GONE)?;
        let recipe = recipe("github").expect("GitHub has a recipe");
        let value = token.expose_secret().trim().to_string();
        if !(recipe.shaped)("token", &value) {
            return Err(
                "That doesn't look like a GitHub token: it starts github_pat_ or ghp_.".into(),
            );
        }
        let ok = self.with(&caller, id, |s| {
            if s.service != "github" || s.connected.is_some() || s.finishing {
                return Ok(false);
            }
            if let Some(flow) = s.flow.take() {
                self.connections.cancel(&s.person, &flow);
            }
            s.flow_state = None;
            s.device = None;
            s.offer = None;
            s.way = Way::Token;
            s.read_at = None;
            s.steps = steps_for(Way::Token, &recipe, &s.whose, None);
            s.secrets.clear();
            s.secrets.insert("token".into(), Secret::new(value));
            s.mark("key", StepState::Done, None);
            Ok(true)
        })?;
        drop(token);
        if ok {
            self.finish(&caller, id).await;
        }
        self.with(&caller, id, |s| Ok(s.view()))
    }

    /// The person pasted their own app's id and secret on setup `id`'s
    /// card: they go no further than this, the vault and the service. Every
    /// step before approving is theirs done, and approving is next.
    pub async fn paste_app(
        self: &Arc<Self>,
        person: &str,
        id: &str,
        client_id: Secret<String>,
        client_secret: Secret<String>,
    ) -> Result<SetupView, String> {
        let caller = self.caller_of(person, id).ok_or(GONE)?;
        let (client_id, client_secret) = (
            client_id.expose_secret().trim().to_string(),
            client_secret.expose_secret().trim().to_string(),
        );
        let ok = self.with(&caller, id, |s| {
            if s.way != Way::App || s.flow.is_some() || s.connected.is_some() || s.finishing {
                return Ok(false);
            }
            let app = super::app::of(s.service).expect("an app's setup has an app");
            if !app.shaped(&client_id, &client_secret) {
                let c = catalog::find(s.service).expect("in the catalog");
                return Err(format!(
                    "That doesn't look like a {} app's id and secret.",
                    c.name
                ));
            }
            s.secrets.insert("client_id".into(), Secret::new(client_id));
            s.secrets
                .insert("client_secret".into(), Secret::new(client_secret));
            let ids: Vec<&'static str> = s
                .steps
                .iter()
                .map(|x| x.id)
                .take_while(|x| *x != "approve")
                .collect();
            for step in ids {
                s.mark(step, StepState::Done, None);
            }
            Ok(true)
        })?;
        if ok {
            self.advance(&caller, id).await?;
        }
        self.with(&caller, id, |s| Ok(s.view()))
    }

    /// The person did, on setup `id`'s card, the step of the app's it
    /// showed them: it is done, and the agent is told to go on. FreeAgent's
    /// new address added is kept as the app's.
    pub async fn done_step(self: &Arc<Self>, person: &str, id: &str) -> Result<SetupView, String> {
        let caller = self.caller_of(person, id).ok_or(GONE)?;
        let done = self.with(&caller, id, |s| {
            let step = s
                .steps
                .iter()
                .find(|x| {
                    x.state == StepState::Waiting
                        && !matches!(x.id, "sign_in" | "key" | "approve" | "check")
                })
                .map(|x| x.id);
            if let Some(step) = step {
                s.mark(step, StepState::Done, Some("You did this step".into()));
                s.said_done = Some(step);
            }
            Ok(step.map(|step| (step, s.service)))
        })?;
        if let Some(("address", service)) = done {
            let c = catalog::find(service).expect("in the catalog");
            self.connections
                .keep_app_address(person, c)
                .map_err(refused)?;
        }
        self.advance(&caller, id).await?;
        self.with(&caller, id, |s| Ok(s.view()))
    }

    /// What the agent does next on setup `id` of the person's own app, now
    /// that nothing waits on the person: the step under way, exactly.
    fn app_next(&self, view: &SetupView) -> Value {
        let Some(x) = view.steps.iter().find(|x| x.state != StepState::Done) else {
            return json!({ "next": "Call connections.wait: Desk checks it and turns it on itself." });
        };
        let todo = x.instruction.clone().unwrap_or_else(|| x.label.clone());
        match x.id {
            "key" => {
                json!({ "step": x.id, "do": todo, "next": "Then call connections.wait: Desk keeps the app and asks the person to approve it." })
            }
            "approve" | "check" => {
                json!({ "next": "Call connections.wait, and nothing else: Desk carries on by itself." })
            }
            _ => json!({
                "step": x.id,
                "do": todo,
                "next": format!("Do this in your browser. Then call connections.step {{setup: \"{}\", step: \"{}\", state: \"done\"}} and connections.wait. If the page isn't as described, or a click fails twice, call connections.keep {{setup: \"{}\", field: \"stuck\", value: <what you saw>}}: the card shows the person this step, and connections.wait tells you once they've done it.", view.id, x.id, view.id),
            }),
        }
    }

    /// Whether the agent's tab shows the person signed in to setup `id`'s
    /// service, for the person's own app: on its console (Google's) or its
    /// apps (FreeAgent's), with no password asked. `None`: no tab.
    async fn app_signed_in(&self, caller: &Caller, service: &str) -> Option<bool> {
        let TabAt { url, password } = self.tabs.location(&caller.agent, &caller.person).await?;
        let parsed = url::Url::parse(&url).ok()?;
        let origin = workforce_skills::adapters::browser::origin_of(parsed.as_str());
        let held = match service {
            "freeagent" => {
                origin.as_deref() == Some(FREEAGENT_DEV) && parsed.path().starts_with("/apps")
            }
            _ => origin.as_deref() == Some(GOOGLE_CONSOLE),
        };
        Some(held && !password)
    }

    /// Waits, at most `most`, while setup `id` waits on the person: their
    /// word on its card, GitHub's code approved, or their signing in on the
    /// key page in the agent's tab, which is watched here. What the agent
    /// does next.
    pub async fn wait(&self, caller: &Caller, id: &str, most: Duration) -> Result<Value, String> {
        self.check_caller(caller)?;
        let until = Instant::now() + most;
        let every = self
            .connections
            .services
            .poll_floor
            .clamp(Duration::from_millis(5), Duration::from_secs(1));
        let mut shown = false;
        loop {
            let (view, held, recipe_of, said, did) = self.with(caller, id, |s| {
                let c = catalog::find(s.service).expect("in the catalog");
                let held = secret_fields(c).iter().all(|f| s.secrets.contains_key(*f));
                Ok((
                    s.view(),
                    held || s.finishing,
                    s.service,
                    std::mem::take(&mut s.said_signed_in),
                    s.said_done.take(),
                ))
            })?;
            match view.now.kind {
                NowKind::Ended => {
                    let next = match view.ended {
                        Some(Ended::Stopped) => "This setup was stopped. Say nothing more about it unless the person asks to start again: then call connections.setup.",
                        _ => "A newer setup of this service took over. Say nothing more about this one.",
                    };
                    return Ok(json!({ "ended": true, "next": next }));
                }
                NowKind::Connected => {
                    let c = view.connected.as_ref();
                    return Ok(json!({
                        "connected": c.map(|c| c.account.clone()),
                        "onFor": c.and_then(|c| c.on_for.clone()),
                        "write": view.write,
                        "next": "It's connected and on. The card shows it: tell the person in one short line, no more.",
                    }));
                }
                NowKind::Failed => {
                    return Ok(json!({
                        "failed": true,
                        "next": "It didn't connect. The card tells the person what happened and what to do: say so in one line and stop. Don't try another way unless they ask.",
                    }))
                }
                _ => {}
            }
            if let (Way::App, Some(step)) = (view.way, did) {
                let mut next = self.app_next(&view);
                next["did"] = json!(step);
                next["said"] = json!("The person did that step themselves.");
                return Ok(next);
            }
            if view.way == Way::App && view.connected.is_none() {
                let waiting = view.steps.iter().find(|x| x.state == StepState::Waiting);
                match waiting.map(|x| x.id) {
                    Some("approve") => {}
                    Some("sign_in") | None
                        if view
                            .steps
                            .iter()
                            .any(|x| x.id == "sign_in" && x.state != StepState::Done) =>
                    {
                        let page = recipe(recipe_of)
                            .and_then(|r| (r.page)(&BTreeMap::new()))
                            .unwrap_or_default();
                        match self.app_signed_in(caller, recipe_of).await {
                            None => {
                                return Ok(
                                    json!({ "next": format!("Desk's browser has no page open: open {} with browser.navigate, then call connections.wait.", view.steps.iter().find_map(|x| x.url.clone()).unwrap_or(page)) }),
                                )
                            }
                            Some(true) => {
                                self.with(caller, id, |s| {
                                    s.mark("page", StepState::Done, None);
                                    s.mark("sign_in", StepState::Done, None);
                                    Ok(())
                                })?;
                                let view = self.with(caller, id, |s| Ok(s.view()))?;
                                let mut next = self.app_next(&view);
                                next["signedIn"] = json!(true);
                                return Ok(next);
                            }
                            Some(false) => {
                                if waiting.is_none() {
                                    self.with(caller, id, |s| {
                                        s.mark("page", StepState::Done, None);
                                        s.mark("sign_in", StepState::Waiting, None);
                                        Ok(())
                                    })?;
                                }
                                if !shown {
                                    shown = true;
                                    self.tabs.show(&caller.agent).await;
                                }
                            }
                        }
                    }
                    // A step the person is doing on the card.
                    Some(_) => {}
                    None if !view.steps.iter().any(|x| x.state == StepState::Failed) && !held => {
                        return Ok(self.app_next(&view))
                    }
                    None => {}
                }
            }
            if view.way == Way::Browser && !held && said {
                return Ok(json!({
                    "signedIn": true,
                    "next": "The person says they're signed in. Open the key page with browser.navigate if it isn't open, make the key, read it with browser.capture_key {connector} (no selector), then call connections.wait again.",
                }));
            }
            if view.way == Way::Browser && !held {
                let page = self.page_of(caller, id, recipe_of);
                let at = self.tabs.location(&caller.agent, &caller.person).await;
                let open = (recipe(recipe_of).and_then(|r| (r.page)(&BTreeMap::new())))
                    .unwrap_or_else(|| "the service's key page".into());
                match (page, at) {
                    (Some(page), Some(TabAt { url, password })) => {
                        let parsed = url::Url::parse(&url).ok();
                        let origin = parsed.as_ref().and_then(|u| {
                            workforce_skills::adapters::browser::origin_of(u.as_str())
                        });
                        let path = parsed
                            .as_ref()
                            .map(|u| u.path().to_string())
                            .unwrap_or_default();
                        // GitHub asks for the password again on the key
                        // page itself: that is the person's to do.
                        if origin.as_deref() == Some(page.origin.as_str())
                            && page.holds(&path)
                            && !password
                        {
                            self.with(caller, id, |s| {
                                for step in ["where", "page", "sign_in"] {
                                    if s.steps
                                        .iter()
                                        .any(|x| x.id == step && x.state != StepState::Done)
                                    {
                                        s.mark(step, StepState::Done, None);
                                    }
                                }
                                Ok(())
                            })?;
                            return Ok(json!({
                                "signedIn": true,
                                "next": "The person is signed in and the key page is open. Make the key there, then read it with browser.capture_key {connector} (no selector: Desk finds it), then call connections.wait again: Desk checks it and turns it on itself.",
                            }));
                        }
                        let waiting = view.steps.iter().any(|x| x.state == StepState::Waiting);
                        if origin.as_deref() == Some(page.origin.as_str()) {
                            // On the service, not its key page: its sign-in.
                            if !waiting {
                                self.with(caller, id, |s| {
                                    // Before the site is known, or once the
                                    // person said they're in (but for a
                                    // password asked again): not asked again.
                                    if s.steps.iter().any(|x| {
                                        (x.id == "where" && x.state != StepState::Done)
                                            || (x.id == "sign_in"
                                                && x.state == StepState::Done
                                                && !password)
                                    }) {
                                        return Ok(());
                                    }
                                    s.mark("page", StepState::Done, None);
                                    s.mark("sign_in", StepState::Waiting, None);
                                    Ok(())
                                })?;
                            }
                            if !shown {
                                shown = true;
                                self.tabs.show(&caller.agent).await;
                            }
                        } else if !waiting {
                            return Ok(
                                json!({ "next": format!("Open {open} with browser.navigate first, then call connections.wait.") }),
                            );
                        }
                    }
                    (None, _) => {
                        return Ok(
                            json!({ "next": "Keep what the setup still needs with connections.keep first." }),
                        )
                    }
                    (_, None) => {
                        return Ok(
                            json!({ "next": format!("Desk's browser has no page open: open {open} with browser.navigate, then call connections.wait.") }),
                        )
                    }
                }
            }
            if Instant::now() >= until {
                return Ok(match view.now.kind {
                    NowKind::Approve => json!({
                        "waiting": true,
                        "on": view.now.title,
                        "approve": view.now.url,
                        "next": "The card asks the person to approve Desk on the service's page. You may open that page in your tab with browser.navigate; they approve it themselves. Call connections.wait again, and don't message them meanwhile.",
                    }),
                    _ => json!({
                        "waiting": true,
                        "on": view.now.title,
                        "next": "Still waiting on the person. Call connections.wait again. Don't message them: the card says what to do.",
                    }),
                });
            }
            tokio::time::sleep(every).await;
        }
    }

    /// Why `caller` mustn't open a sign-in at `url`: a browser setup in its
    /// conversation is for that site, and its card asks the person.
    pub fn sign_in_owned(&self, caller: &Caller, url: &str) -> Option<String> {
        let origin = workforce_skills::adapters::browser::origin_of(url)?;
        let setups = self.setups.lock().expect("setups poisoned");
        setups
            .values()
            .filter(|s| {
                s.person == caller.person
                    && s.by == caller.agent
                    && s.session == caller.session
                    && s.ended.is_none()
                    && s.connected.is_none()
                    && s.until > Instant::now()
                    && !s.steps.iter().any(|x| x.state == StepState::Failed)
            })
            .find(|s| match s.way {
                Way::Browser | Way::App => recipe(s.service)
                    .is_some_and(|r| (r.origins)(&s.fields).contains(&origin)),
                // GitHub's own sign-in, or the CLI's: no browser at all.
                Way::Cli | Way::Device | Way::Token => origin == "https://github.com",
            })
            .map(|_| {
                "The setup's card in this conversation asks the person to sign in, and Desk sees when they have: don't open a sign-in. Open the key page with browser.navigate and call connections.wait.".to_string()
            })
    }

    /// What the agent does next, as `connections.setup` tells it: first,
    /// where the person finds the setup's card.
    pub fn next(&self, view: &SetupView) -> String {
        let c = catalog::find(view.service).expect("in the catalog");
        let secrets = secret_fields(c);
        let plain: Vec<&str> = c
            .fields
            .iter()
            .filter(|f| !f.secret)
            .map(|f| f.name)
            .collect();
        let said = say_card(view);
        match view.way {
            Way::Cli => return format!("GitHub's CLI is signed in on this computer: the setup's card asks the person to use that sign-in. {said} Then call connections.wait, and nothing else: Desk checks it and turns it on itself. Don't open a browser or say anything more to the person."),
            Way::Device => return format!("The setup's card shows the person a code to approve on GitHub. {said} Then call connections.wait, and nothing else: Desk finishes by itself once they approve. Don't open a browser, read the code out or say anything more to the person."),
            Way::Browser => {}
            Way::Token => return format!("The person is pasting a token of their own on the setup's card. {said} Then call connections.wait, and nothing else."),
            Way::App => return self.app_plan(view, &said),
        }
        let page = match view.service {
            "github" => github_token_page(&view.name, view.write),
            _ => recipe(view.service)
                .and_then(|r| (r.page)(&BTreeMap::new()))
                .unwrap_or_else(|| "the service's key page".into()),
        };
        let making = match view.service {
            "github" => " The page comes filled in: choose \"All repositories\" under Repository access, then Generate token and confirm.",
            "bank" => " On BankSync's Developers page: Create Key, Key Name as above, Expiration 1 year, under Permissions only banks:read and institutions:read, then Create API Key. BankSync shows the key once, on the view that follows: read it there before Done (press its show button first if it's masked).",
            _ => "",
        };
        format!(
            "{said}\n\
             1. Keep {} with connections.keep (ask the person if you can't tell).\n\
             2. Open {} with browser.navigate, then call connections.wait. If the person must sign in, the card asks them and Desk sees when they have: never call browser.sign_in, never type or read a password.\n\
             3. Once signed in, make the key (name it like \"Desk · {}\").{making} The page hides keys from you: read {} with browser.capture_key {{connector: \"{}\"}}, with no selector: Desk finds it. The key never comes back to you. Make and read it in this turn: Desk's tab closes when your turn ends.\n\
             4. Call connections.wait: Desk checks it with the service and turns it on itself.\n\
             If the person makes the key themselves or has one already, call connections.step {{setup, step: \"key\", state: \"waiting\"}}: the card asks them to send it with `cmd: secret {}_key <the key>`, and the setup waits for them across turns. Tell them so and end your turn: once it arrives Desk keeps it, checks it and turns it on by itself, and tells you how that went, so they never have to say it's sent. Where they sent it already, call connections.keep {{setup, field: \"{}\", value: \"[[secret:{}_key]]\"}}, then connections.wait.",
            if plain.is_empty() { "nothing".to_string() } else { plain.join(", ") },
            page,
            view.name,
            secrets.join(" and "),
            view.service,
            view.service,
            secrets.first().copied().unwrap_or("api_key"),
            view.service,
        )
    }

    /// What the agent does for a setup of the person's own app: every step
    /// it drives in their Chrome, in order, or only waiting on their
    /// approval where the app is kept already.
    fn app_plan(&self, view: &SetupView, said: &str) -> String {
        if view.steps.first().is_some_and(|x| x.id == "approve") {
            return format!("The person's {} app is set up already: the setup's card asks them to approve Desk on {}'s page. {said} Then call connections.wait, and nothing else: Desk finishes by itself once they approve.", view.service_name, view.service_name);
        }
        let steps: Vec<String> = view
            .steps
            .iter()
            .filter(|x| !matches!(x.id, "approve" | "check"))
            .enumerate()
            .map(|(n, x)| {
                format!(
                    "{}. [{}] {}",
                    n + 1,
                    x.id,
                    x.instruction.as_deref().unwrap_or(&x.label)
                )
            })
            .collect();
        format!(
            "{said}\n\
             You set up the person's own {service} app in their Chrome, through Desk's browser: Desk ships no app of its own. The person only signs in and approves; you do every other step. Keep the email they sign in with first with connections.keep {{field: \"email\"}} if you know it.\n\
             {}\n\
             After each step, call connections.step {{setup, step, state: \"done\"}}; call connections.wait whenever the person has something to do: it says what's next. Never call browser.sign_in, never type or read a password. If Chrome isn't connected, a page isn't as described, or a click fails twice, call connections.keep {{setup, field: \"stuck\", value: <what you saw>}}: the card then shows the person that step, exactly, and connections.wait tells you once it's done. Once the keys are held, Desk asks the person to approve on {service}'s page, then checks it and turns it on itself.",
            steps.join("\n"),
            service = view.service_name,
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
            if matches!(step, "approve" | "key" | "address") && s.way == Way::App {
                return Err("Desk marks that step itself: read the keys with browser.capture_key, and the person approves on the service's page.".into());
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
            if s.way == Way::App {
                return self.keep_app_field(s, c, field, value);
            }
            let f = c
                .fields
                .iter()
                .find(|f| f.name == field)
                .ok_or_else(|| format!("{} has no field {field}", c.name))?;
            if f.secret {
                return Err(format!("{} is a key. {GIVE_A_KEY}", f.label));
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
                let found = match s.service {
                    "bank" => "The bank",
                    _ => "Found your site",
                };
                s.relabel("where", format!("{found}: {value}"));
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

    /// [`Self::keep`] for a setup of the person's own app: the email they
    /// sign in with, FreeAgent's new address added to their app, or that
    /// the step under way is the person's (`stuck`, in the agent's words).
    fn keep_app_field(
        &self,
        s: &mut Setup,
        c: &Connector,
        field: &str,
        value: &str,
    ) -> Result<SetupView, String> {
        let value = value.trim();
        if value.is_empty() || value.len() > MAX_FIELD || value.chars().any(char::is_control) {
            return Err(format!("{field} should be one short line"));
        }
        match field {
            "email" if value.contains('@') => {
                s.fields.insert("email".into(), value.to_string());
                if let Some(x) = s.steps.iter_mut().find(|x| x.id == "sign_in") {
                    if !x.label.contains(" as ") {
                        x.label = format!("{} as {value}", x.label);
                    }
                }
            }
            "email" => return Err("That isn't an email address".into()),
            "redirect_uri_added" => {
                if !s
                    .steps
                    .iter()
                    .any(|x| x.id == "address" && x.state != StepState::Done)
                {
                    return Err("This setup has no address to add.".into());
                }
                self.connections
                    .keep_app_address(&s.person, c)
                    .map_err(refused)?;
                s.mark("address", StepState::Done, None);
            }
            "stuck" => {
                let step = s
                    .steps
                    .iter()
                    .find(|x| {
                        x.state != StepState::Done
                            && !matches!(x.id, "sign_in" | "approve" | "check")
                    })
                    .map(|x| x.id)
                    .ok_or("No step of this setup is the person's to do now.")?;
                let note: String = super::scrub(value, &BTreeMap::new())
                    .chars()
                    .take(200)
                    .collect();
                s.mark(step, StepState::Waiting, Some(note));
            }
            _ => {
                return Err(format!(
                    "A {} setup keeps email, stuck or redirect_uri_added",
                    c.name
                ))
            }
        }
        Ok(s.view())
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
                    && matches!(s.way, Way::Browser | Way::App)
                    && s.flow.is_none()
                    && s.ended.is_none()
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
                fields: secret_fields(c)
                    .into_iter()
                    .filter(|f| !s.secrets.contains_key(*f))
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
    /// anywhere: once every key is held, Desk checks it with the service and
    /// turns it on ([`Self::finish_when_ready`]). What comes back holds none
    /// of it.
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
        let page = self
            .page_of(caller, &id, connector)
            .ok_or("That setup has ended.")?;
        if read_origin != page.origin || !page.holds(read_path) {
            return Err("That wasn't the service's own key page, so nothing was kept.".into());
        }
        let read = Secret::new(value.expose_secret().clone());
        let kept = self.hold(caller, &id, connector, field, value, Some(page.origin))?;
        // Should it come back into view, it reaches the model as a marker.
        if let (Some(cache), Some(key)) = (
            workforce_common::secret_marker::global_cache(),
            caller.session_key.as_deref(),
        ) {
            cache.capture(
                key,
                read,
                "CONNECTION_KEY",
                workforce_common::secret_marker::SecretSource::ToolResult {
                    tool: "browser.capture_key".into(),
                },
            );
        }
        Ok(kept)
    }

    /// Keeps key `field` of setup `id` from `marker`, a secret the person
    /// sent in this conversation with `cmd: secret`: the hub
    /// resolves it, so the key never passes through the model, and it is
    /// held exactly as one read off the service's page. A raw value, a
    /// marker this conversation doesn't hold, or one scraped from a page or
    /// a tool's answer is refused.
    pub fn keep_given(
        &self,
        caller: &Caller,
        id: &str,
        field: &str,
        marker: &str,
    ) -> Result<KeptKey, String> {
        self.keep_handed(caller, id, Some(field), marker)
    }

    /// [`Self::keep_given`], into key `field`, else the next still to hold
    /// that it is shaped for.
    fn keep_handed(
        &self,
        caller: &Caller,
        id: &str,
        field: Option<&str>,
        marker: &str,
    ) -> Result<KeptKey, String> {
        self.check_caller(caller)?;
        let connector = self.with(caller, id, |s| {
            let c = catalog::find(s.service).expect("in the catalog");
            if let Some(field) = field.filter(|f| !secret_fields(c).contains(f)) {
                return Err(format!("{field} isn't one of {}'s keys", c.name));
            }
            if key_bound_to_page(s.service) {
                return Err(format!("{}'s key is read off its own site with browser.capture_key, so the site it goes to is the one it came from.", c.name));
            }
            if !matches!(s.way, Way::Browser | Way::App) {
                return Err("This setup doesn't take a key from the conversation: its card says what the person does.".into());
            }
            if s.ended.is_some() {
                return Err("That setup has ended: start it again with connections.setup, then keep the key.".into());
            }
            Ok(s.service)
        })?;
        if self.running(caller, connector).as_deref() != Some(id) {
            return Err(format!("This {connector} setup isn't waiting on a key now, so nothing was kept: Desk is already checking one, or a newer setup took its place."));
        }
        let value = given_secret(caller, marker)?;
        self.hold(caller, id, connector, field, value, None)
    }

    /// The person sent `name` with `cmd: secret` in `conversation`. Where a
    /// setup of theirs there takes a key in the conversation and `name` is
    /// one of its keys' (`<service>_key`, or `<service>_<field>`), it is kept
    /// as that key, checked with the service and turned on, and the agent
    /// running it is told how that went, so it carries on with nothing more
    /// said. What the person is told of it, where a setup took it.
    pub fn key_sent(
        self: &Arc<Self>,
        conversation: &str,
        person: &str,
        name: &str,
    ) -> Option<String> {
        let (caller, id, service, field) = self.taking(conversation, person, name)?;
        let kept = self.keep_handed(&caller, &id, field, &format!("[[secret:{name}]]"));
        let claimed = match &kept {
            Ok(k) if k.ready => self.claim(&caller, &id),
            _ => None,
        };
        let c = catalog::find(service).expect("in the catalog");
        let told = match &kept {
            Ok(k) if k.ready => format!("Desk is checking it with {} and turning it on.", c.name),
            Ok(k) => format!("Desk kept it as {}'s {}; it {}.", c.name, k.field, k.test),
            Err(e) => format!("Desk didn't keep it as {}'s key: {e}", c.name),
        };
        let (this, name) = (Arc::clone(self), name.to_string());
        tokio::spawn(async move {
            if let Some(write) = claimed {
                this.finish_claimed(&caller, &id, write).await;
            }
            let outcome = match kept {
                Ok(k) if k.ready => this
                    .with(&caller, &id, |s| Ok(sent_outcome(&s.view())))
                    .unwrap_or_else(|_| "The setup ended before Desk could check it.".into()),
                Ok(k) => format!("It is kept as its {}, and the setup {}: ask the person for the rest the same way.", k.field, k.test),
                Err(_) => format!("Desk didn't keep it as {}'s key: ask the person to send it again with `cmd: secret {service}_key <the key>`.", c.name),
            };
            this.callers.tell(
                &caller,
                format!("The person sent {}'s key with `cmd: secret {name}` for the setup [{id}], and Desk took it straight to the setup, never through you. {outcome}", c.name),
            );
        });
        Some(told)
    }

    /// The setup in `conversation` of `person`'s that a key `name` they
    /// sent there is for, with its caller and the key's field where `name`
    /// names one.
    #[allow(clippy::type_complexity)]
    fn taking(
        &self,
        conversation: &str,
        person: &str,
        name: &str,
    ) -> Option<(Caller, String, &'static str, Option<&'static str>)> {
        let setups = self.setups.lock().expect("setups poisoned");
        let now = Instant::now();
        setups
            .values()
            .filter(|s| {
                s.person == person
                    && s.conversation.as_deref() == Some(conversation)
                    && s.way == Way::Browser
                    && s.flow.is_none()
                    && s.until > now
                    && s.ended.is_none()
                    && s.connected.is_none()
                    && !s.finishing
                    && !key_bound_to_page(s.service)
            })
            .filter_map(|s| Some((s, key_named(s.service, name)?)))
            .max_by_key(|(s, _)| s.started_at)
            .map(|(s, field)| {
                let caller = Caller {
                    person: s.person.clone(),
                    agent: s.by.clone(),
                    session: s.session.clone(),
                    session_key: s.conversation.clone(),
                    overseer: s.overseer,
                };
                (caller, s.id.clone(), s.service, field)
            })
    }

    /// Holds `value` as key `field` of setup `id` (the next one still to
    /// read where none is named, by its shape), read on `read_at` where it
    /// was read off a page.
    fn hold(
        &self,
        caller: &Caller,
        id: &str,
        connector: &str,
        field: Option<&str>,
        value: Secret<String>,
        read_at: Option<String>,
    ) -> Result<KeptKey, String> {
        let recipe = recipe(connector).ok_or_else(|| by_hand(connector).to_string())?;
        let page = self
            .page_of(caller, id, connector)
            .ok_or("That setup has ended.")?;
        let c = catalog::find(connector).expect("in the catalog");
        let field = match field {
            Some(f) => secret_fields(c)
                .into_iter()
                .find(|x| *x == f)
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
            return Err(format!(
                "That doesn't look like a {} key, so nothing was kept.",
                c.name
            ));
        }
        let length = value.expose_secret().chars().count();
        let all = self.with(caller, id, move |s| {
            // An app's id and secret may be shaped alike: the same value
            // read for both is neither.
            if c.sign_in == catalog::SignIn::App
                && s.secrets
                    .iter()
                    .any(|(k, v)| k != field && v.expose_secret() == value.expose_secret())
            {
                return Err(format!(
                    "That's the value already kept as the app's {}, so nothing was kept. Read the {field} with browser.capture_key and field: \"{field}\".",
                    APP_FIELDS.iter().find(|f| **f != field).unwrap_or(&field)
                ));
            }
            s.secrets.insert(field.to_string(), value);
            s.read_at = read_at;
            let all = secret_fields(c).iter().all(|f| s.secrets.contains_key(*f));
            if all {
                // Everything before the keys is done once they are held.
                let before: Vec<&'static str> = s
                    .steps
                    .iter()
                    .map(|x| x.id)
                    .take_while(|x| *x != "approve" && *x != "check")
                    .collect();
                for step in before {
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
            true if c.sign_in == catalog::SignIn::App => "kept: Desk keeps the person's app and asks them to approve it on the service's page. Call connections.wait.".to_string(),
            true => "kept: Desk checks it with the service and turns it on itself. Call connections.wait.".to_string(),
        };
        Ok(KeptKey {
            kept: true,
            connector: c.id,
            field,
            length,
            test,
            ready: all,
            setup: id.to_string(),
        })
    }

    /// Once `kept` was the last key its setup needed, Desk checks it with
    /// the service and turns it on, as the setup's start said it would,
    /// while the agent waits.
    pub fn finish_when_ready(self: &Arc<Self>, caller: &Caller, kept: &KeptKey) {
        if kept.ready {
            let (this, caller, id) = (Arc::clone(self), caller.clone(), kept.setup.clone());
            tokio::spawn(async move { this.finish(&caller, &id).await });
        }
    }

    /// Checks what the setup holds and, on a pass, keeps it at its tier.
    pub async fn test(&self, caller: &Caller, id: &str) -> Result<SetupView, String> {
        self.check_caller(caller)?;
        let (service, level, key, name, fields, flow, read_at, way) =
            self.with(caller, id, |s| {
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
                    s.way,
                ))
            })?;
        // A token the agent made for GitHub is a fine-grained one, made on
        // the page Desk filled in; a classic one could carry any scope.
        if service == "github"
            && way == Way::Browser
            && flow.is_none()
            && fields
                .get("token")
                .is_some_and(|t| !t.trim().starts_with("github_pat_"))
        {
            self.with(caller, id, |s| {
                s.secrets.clear();
                s.read_at = None;
                s.mark(
                    "check",
                    StepState::Failed,
                    Some(
                        "The token wasn't a fine-grained one made on the page Desk filled in."
                            .into(),
                    ),
                );
                Ok(())
            })?;
            return Err("Nothing was kept: make a fine-grained token on the page Desk opened, never a classic one.".into());
        }
        // Where the key goes: the site kept for it, checked again, and
        // Metabase's only where its key was read.
        if flow.is_none() {
            for (svc, f) in [("jira", "site"), ("metabase", "site_url")] {
                if service == svc {
                    let site = fields
                        .get(f)
                        .ok_or("Keep the site with connections.keep first.")?;
                    let origin = site_origin(svc, site)?;
                    if key_bound_to_page(svc) && read_at.as_deref() != Some(origin.as_str()) {
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
                // Where the sign-in ended: an account connected already is
                // its own connection, signed in again.
                Some(super::FlowState::Connected { account }) => (
                    account,
                    self.connections.flow_conn(&flow).unwrap_or(Conn {
                        level: level.clone(),
                        key: key.clone(),
                    }),
                ),
                Some(super::FlowState::Waiting) => {
                    return Err(format!(
                        "{} is still waiting for the person to approve it.",
                        catalog::find(service).map_or(service, |c| c.name)
                    ))
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
                let account = match self.connections.check(c, &person, &fields).await {
                    Found::Works { account } => account,
                    found @ (Found::Refused(_) | Found::Failed(_)) => {
                        // The service's own words go on the person's card,
                        // never back to the agent.
                        let (said, told) = match &found {
                            Found::Refused(e) => (e, format!("{} didn't accept the key.", c.name)),
                            Found::Failed(e) => (e, format!("{} couldn't be reached.", c.name)),
                            Found::Works { .. } => unreachable!(),
                        };
                        let note: String = super::scrub(said, &super::secrets_of(c, &fields))
                            .chars()
                            .take(200)
                            .collect();
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
        if s.way == Way::App {
            return match field {
                Some("stuck") => "that the step under way is the person's to do".into(),
                Some("redirect_uri_added") => {
                    "that Desk's new address is on the person's FreeAgent app".into()
                }
                Some("email") => format!(
                    "the email signed in with: {} characters",
                    value.unwrap_or_default().chars().count()
                ),
                _ => UNKNOWN.into(),
            };
        }
        let Some(f) =
            catalog::find(s.service).and_then(|c| c.fields.iter().find(|f| Some(f.name) == field))
        else {
            return UNKNOWN.into();
        };
        let value = value.unwrap_or_default();
        if f.secret {
            return match is_marker(value) {
                true => format!("{}: the key the person handed over", f.label),
                false => UNKNOWN.into(),
            };
        }
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
            .values_mut()
            .filter(|s| s.person == person && agent.is_none_or(|a| s.by == a))
            .map(|s| {
                s.sync(&self.connections);
                s.view()
            })
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
            s.said_signed_in = true;
        }
        Some(s.view())
    }

    /// The agent stops setup `id` (`connections.stop`), as the person asked
    /// or as it gave up on it.
    pub fn stop(&self, caller: &Caller, id: &str) -> Result<SetupView, String> {
        self.with(caller, id, |s| Ok(self.stop_one(s)))
    }

    /// The person stops setup `id` on its card.
    pub fn stop_for(&self, person: &str, id: &str) -> Result<SetupView, String> {
        let caller = self.caller_of(person, id).ok_or(GONE)?;
        self.stop(&caller, id)
    }

    /// Ends `s` as stopped, unless it is past stopping: connected, or being
    /// checked and turned on now, which finishes in a moment.
    fn stop_one(&self, s: &mut Setup) -> SetupView {
        if s.ended.is_none() && s.connected.is_none() && !s.finishing {
            s.end(Ended::Stopped, &self.connections);
        }
        s.view()
    }

    /// A turn of conversation `session` ended: each of its setups that was
    /// the agent's to carry on is stopped, so none shows as under way with
    /// nobody doing it. One waiting on the person, the service or Desk's
    /// own check goes on.
    pub fn turn_ended(&self, session: &str) {
        let mut setups = self.setups.lock().expect("setups poisoned");
        for s in setups
            .values_mut()
            .filter(|s| s.session.as_deref() == Some(session))
        {
            s.sync(&self.connections);
            if s.agents_to_do() {
                s.end(Ended::Stopped, &self.connections);
            }
        }
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

/// What a card says of a setup that has gone.
const GONE: &str = "That setup has ended. Ask the agent to start again.";

/// A setup's steps, the way it gets its key.
fn steps_for(
    way: Way,
    recipe: &Recipe,
    whose: &str,
    offer: Option<&super::gh::Login>,
) -> Vec<Step> {
    let mut steps = Vec::new();
    match way {
        Way::Cli => steps.push(step(
            "use",
            &format!(
                "Use the GitHub sign-in on this computer{}",
                offer
                    .map(|o| format!(" as {}", o.account))
                    .unwrap_or_default()
            ),
        )),
        Way::Device => steps.push(step("sign_in", "Approve Desk on GitHub")),
        Way::Token => steps.push(step("key", "Paste a token you made on GitHub")),
        Way::Browser | Way::App => {
            if let Some(label) = recipe.where_label {
                steps.push(step("where", label));
            }
            steps.push(step("page", recipe.page_label));
            steps.push(step("sign_in", recipe.sign_in_label));
            steps.push(step("key", recipe.key_label));
        }
    }
    steps.push(step(
        "check",
        &format!("Check it works, then turn it on for {whose}"),
    ));
    steps[0].state = StepState::Current;
    steps
}

/// GitHub's page for a new fine-grained token, filled in through the
/// parameters GitHub documents: its name, 90 days, and the least the
/// agent's GitHub tools need (read, or write where it takes writes).
fn github_token_page(name: &str, write: bool) -> String {
    let level = match write {
        true => "write",
        false => "read",
    };
    let token_name: String = format!("Desk · {name}").chars().take(40).collect();
    let mut u = url::Url::parse("https://github.com/settings/personal-access-tokens/new")
        .expect("GitHub's token page parses");
    u.query_pairs_mut()
        .append_pair("name", &token_name)
        .append_pair(
            "description",
            "Made by Desk for its agents. Kept in the vault on this computer.",
        )
        .append_pair("expires_in", "90")
        .append_pair("contents", level)
        .append_pair("pull_requests", level)
        .append_pair("issues", level)
        .append_pair("metadata", "read");
    u.to_string()
}

fn step(id: &'static str, label: &str) -> Step {
    Step {
        id,
        label: label.to_string(),
        state: StepState::Todo,
        note: None,
        instruction: None,
        url: None,
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
    #[cfg(test)]
    if let Some(setups) = testing::ON_THIS_THREAD.with(|s| s.borrow().clone()) {
        return Some(setups);
    }
    INSTALLED.get().cloned()
}

#[cfg(test)]
pub(crate) mod testing {
    use std::cell::RefCell;

    use super::*;

    thread_local! {
        pub(super) static ON_THIS_THREAD: RefCell<Option<Arc<Setups>>> = const { RefCell::new(None) };
    }

    /// `setups` as the hub's, for the test on this thread (a current-thread
    /// runtime's tasks with it) until dropped.
    pub(crate) struct Installed;

    impl Installed {
        pub(crate) fn install(setups: Arc<Setups>) -> Self {
            ON_THIS_THREAD.with(|s| *s.borrow_mut() = Some(setups));
            Self
        }
    }

    impl Drop for Installed {
        fn drop(&mut self) {
            ON_THIS_THREAD.with(|s| s.borrow_mut().take());
        }
    }
}

/// The hub's callers, from its registry and identities.
struct Hub(Arc<crate::gateway::GatewayState>);

impl Callers for Hub {
    fn holds_the_hub(&self, holder: crate::authz::Holder<'_>) -> bool {
        crate::desk_floor::holds_the_hub(self.0.identity_store.as_ref(), true, Some(holder))
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

    fn tell(&self, caller: &Caller, note: String) {
        let Some(service) = self.0.registry.get(&caller.agent) else {
            return;
        };
        let (Some(crew), Some(conversation)) = (
            crate::desk_crew::for_service(&service),
            caller.session_key.clone(),
        ) else {
            return;
        };
        let (person, session) = (caller.person.clone(), caller.session.clone());
        tokio::spawn(async move {
            // Queued behind the conversation's turn (the one the key came in
            // on, or the agent's own): told once it has ended, never on top.
            let busy = || {
                session
                    .as_deref()
                    .is_some_and(|s| service.live_turns.running(s))
            };
            let until = Instant::now() + SETUP_TTL;
            while Instant::now() < until {
                if busy() {
                    tokio::time::sleep(TELL_EVERY).await;
                    continue;
                }
                match crew
                    .run_turn(&service, &person, &conversation, &note, 0, None, None)
                    .await
                {
                    // Another turn began first: it waits for that one too.
                    Err(_) if busy() => continue,
                    Err(e) => {
                        tracing::warn!(agent = %service.agent_id, error = %e, "Desk couldn't tell the agent what became of a key the person sent");
                        return;
                    }
                    Ok(_) => return,
                }
            }
            tracing::warn!(agent = %service.agent_id, "The conversation stayed busy, so Desk didn't tell the agent what became of a key the person sent");
        });
    }
}

/// How often Desk looks for the conversation's turn to end before it tells
/// the agent there.
const TELL_EVERY: Duration = Duration::from_millis(250);

/// Setups over `connections`.
pub fn install(
    state: &Arc<crate::gateway::GatewayState>,
    connections: Arc<Connections>,
) -> Arc<Setups> {
    let setups = INSTALLED
        .get_or_init(|| Arc::new(Setups::new(connections, Arc::new(Hub(Arc::clone(state))))))
        .clone();
    register_key_pages();
    setups
}

/// The setups' routes.
pub fn routes(
    state: &Arc<crate::gateway::GatewayState>,
    setups: crate::surface::Bound<Arc<Setups>>,
) -> crate::surface::Routes {
    crate::surface::Routes::new()
        .admin("/api/desk/connection-setups", get(list_route))
        .admin(
            "/api/desk/connection-setups/{id}/signed-in",
            post(signed_in_route),
        )
        .admin("/api/desk/connection-setups/{id}/token", post(token_route))
        .admin("/api/desk/connection-setups/{id}/app", post(app_route))
        .admin(
            "/api/desk/connection-setups/{id}/{choice}",
            post(choice_route),
        )
        .bind(setups.map(|setups| {
            Arc::new(Routes {
                state: Arc::clone(state),
                setups,
            })
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
    match caller.holder() {
        Some(holder) if routes.setups.callers.holds_the_hub(holder) => {
            Ok(holder.identity_id.to_string())
        }
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

/// What the person chose on a setup's card: `use-cli` (the GitHub CLI's
/// sign-in), `other-way`, `new-code`, `done-step` (a step of their own
/// app's done), or `stop`. Theirs alone: no tool reaches it.
async fn choice_route(
    State(r): State<Arc<Routes>>,
    h: HeaderMap,
    Query(q): Query<ListQuery>,
    Path((id, choice)): Path<(String, String)>,
) -> Response {
    let person = match holder(&r, &h, q.token.as_deref()).await {
        Ok(person) => person,
        Err(refused) => return refused,
    };
    let done = match choice.as_str() {
        "use-cli" => r.setups.use_cli(&person, &id).await,
        "other-way" => r.setups.other_way(&person, &id).await,
        "new-code" => r.setups.new_code(&person, &id).await,
        "done-step" => r.setups.done_step(&person, &id).await,
        "stop" => r.setups.stop_for(&person, &id),
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "There's no such choice." })),
            )
                .into_response()
        }
    };
    match done {
        Ok(view) => Json(json!({ "setup": view })).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct TokenBody {
    token: String,
}

/// A GitHub token the person pasted on a setup's card: theirs alone, from
/// the card straight to the vault. Never logged, never in an answer.
async fn token_route(
    State(r): State<Arc<Routes>>,
    h: HeaderMap,
    Query(q): Query<ListQuery>,
    Path(id): Path<String>,
    Json(body): Json<TokenBody>,
) -> Response {
    let token = Secret::new(body.token);
    let person = match holder(&r, &h, q.token.as_deref()).await {
        Ok(person) => person,
        Err(refused) => return refused,
    };
    match r.setups.paste_token(&person, &id, token).await {
        Ok(view) => Json(json!({ "setup": view })).into_response(),
        Err(e) if e == GONE => (StatusCode::NOT_FOUND, Json(json!({ "error": e }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct AppBody {
    client_id: String,
    client_secret: String,
}

/// The person's own app's id and secret, pasted on a setup's card: theirs
/// alone, from the card straight to the vault. Never logged, never in an
/// answer.
async fn app_route(
    State(r): State<Arc<Routes>>,
    h: HeaderMap,
    Query(q): Query<ListQuery>,
    Path(id): Path<String>,
    Json(body): Json<AppBody>,
) -> Response {
    let (client_id, client_secret) = (Secret::new(body.client_id), Secret::new(body.client_secret));
    let person = match holder(&r, &h, q.token.as_deref()).await {
        Ok(person) => person,
        Err(refused) => return refused,
    };
    match r
        .setups
        .paste_app(&person, &id, client_id, client_secret)
        .await
    {
        Ok(view) => Json(json!({ "setup": view })).into_response(),
        Err(e) if e == GONE => (StatusCode::NOT_FOUND, Json(json!({ "error": e }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
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
            "connections.list" => "The person's connections to services (Jira, Linear, GitHub, Datadog, Metabase, Bank, Email, Google, Slack) you can use: each by the name the person gave it, its site and account, whether it is for all their agents or just you, whether you use it, take writes with it, and which is your default. Never a key.",
            "connections.step" => "Marks a step `current`, `waiting` or `done`. For Google and FreeAgent, mark each step of the person's app done as you finish it; otherwise rarely needed, as the card follows the setup by itself.",
            "connections.wait" => "Wait while a setup waits on the person (using a sign-in on this computer, approving GitHub's code, signing in to the service in your browser, approving Desk on Google's or FreeAgent's page, doing a step on the card): Desk sees when they have, and checks and turns the connection on itself. Returns what to do next, exactly. Call it again while it says it's still waiting; never message the person meanwhile, the card says what to do.",
            "connections.setup" => "Start setting up a connection to a service yourself: Jira, Linear, Datadog, Metabase, GitHub, Google (Calendar, Drive, Gmail), FreeAgent or Bank (one bank the person linked in BankSync: keep `bank`, its name, before the key). `name` is what the person calls it (Acme, Work; for FreeAgent, the business, like Harbor). Each Google account or FreeAgent business is its own connection: set up a second one with its own name. `for` is `me` (default), or, for the person's overseer only, `all` or another agent's name. `write` (default false) lets whom it is for make changes with it. Desk shows the person one card for it in this conversation, and turns it on itself once it works. For Google and FreeAgent you set up the person's own app in their Chrome first, where none is kept; the steps come back exactly. Calling it again for the same connection returns the same setup. Returns the setup's id and what to do next. Email and Slack the person connects themselves.",
            "connections.keep" => "Keep a field of a setup that isn't a secret: a Jira site (acme.atlassian.net), a Metabase address (its https origin), a Datadog site (us1, eu1, …), a bank's name as BankSync shows it (NatWest), an email. For Google or FreeAgent: `email` (the account the person signs in with), `redirect_uri_added` = yes once Desk's new address is on their FreeAgent app, or `stuck` = what you saw, when Chrome isn't connected, a page isn't as described or a click fails twice: the card then shows the person the step, exactly. A key is read off its page with browser.capture_key; one the person already has, sent by them with `cmd: secret <name> <key>` (mark the key step waiting with connections.step while they do), is kept by its [[secret:…]] marker as the value (field: the key's, like api_key): the hub resolves it, so it never passes through you, and checks it with the service as it would a key it read. Once a key is held nothing more is kept.",
            "connections.stop" => "Stop a setup that won't be finished: the person asked you to, or you are giving up on it. Its card goes and nothing it held is kept. One left as your work to carry on stops by itself when your turn ends; one waiting on the person (a sign-in, a code, the key they send) goes on.",
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
                    "step": { "type": "string", "description": "The step's id: where, page, sign_in, key or check; for Google and FreeAgent also project, apis, branding, audience, access, client or make" },
                    "state": { "type": "string", "enum": ["current", "waiting", "done"] },
                    "note": { "type": "string", "description": "A few words for the person" }
                },
                "required": ["setup", "step", "state"]
            }),
            "connections.setup" => json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string", "enum": ["jira", "linear", "datadog", "metabase", "github", "google", "freeagent", "bank"] },
                    "name": { "type": "string" },
                    "for": { "type": "string", "description": "me (default), all, or an agent's name" },
                    "write": { "type": "boolean", "description": "Whether whom it is for makes changes with it (default false)" }
                },
                "required": ["service"]
            }),
            "connections.wait" | "connections.stop" => json!({
                "type": "object",
                "properties": { "setup": setup },
                "required": ["setup"]
            }),
            "connections.keep" => json!({
                "type": "object",
                "properties": {
                    "setup": setup,
                    "field": { "type": "string", "description": "site, email, site_url, bank; a key's (api_key, token, …) with a [[secret:…]] marker; for Google and FreeAgent: email, stuck or redirect_uri_added" },
                    "value": { "type": "string", "description": "The value, or for a key the [[secret:…]] marker the person handed it over as" }
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

    /// Listing, marking a card's step and waiting touch nothing. Starting a
    /// setup is the one connection write the person is asked about: what
    /// follows from it (its key kept, the connection turned on once it
    /// works) is the hub's. Keeping a field, testing and turning on by hand
    /// are writes to Desk's gate too: Allow once or Deny below unbound.
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
            "connections.list" | "connections.step" | "connections.wait" | "connections.stop" => {
                Effect::None
            }
            // Starting a setup is the person's one word for it: its key is
            // kept, and it is turned on for whom it is for, once it works.
            "connections.setup" => {
                write(text(input, "for").is_some_and(|f| f != "me" && f != self.agent))
            }
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
            ("connections.setup", _) => {
                let service = text(input, "service")
                    .and_then(|s| catalog::find(&s))
                    .map_or(UNKNOWN, |c| c.name);
                let whom = match text(input, "for").as_deref() {
                    None | Some("me") => setups
                        .as_ref()
                        .map_or_else(|| UNKNOWN.to_string(), |s| s.agent_name(&self.agent)),
                    Some("all") => "all the person's agents".into(),
                    Some(other) => setups.as_ref().map_or_else(
                        || UNKNOWN.to_string(),
                        |s| {
                            s.callers
                                .agent_named(other)
                                .map_or_else(|| UNKNOWN.to_string(), |id| s.agent_name(&id))
                        },
                    ),
                };
                format!(
                    "Set up {service} for {whom}: keep its key in the vault and turn it on, {}",
                    match input.get("write").and_then(Value::as_bool) {
                        Some(true) => "read and write",
                        _ => "read only",
                    }
                )
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
                        input.get("write").and_then(Value::as_bool).unwrap_or(false),
                    )
                    .await
                    .map(|view| {
                        let next = setups.next(&view);
                        json!({ "setup": view.id, "card": card_link(&view.id), "next": next })
                    }),
            },
            "connections.wait" => match need() {
                Err(e) => Err(e),
                Ok(id) => setups.wait(&caller, &id, WAIT_MOST).await,
            },
            "connections.stop" => need().and_then(|id| {
                setups
                    .stop(&caller, &id)
                    .map(|v| {
                        let next = match (v.ended, v.now.kind) {
                            (Some(_), _) => "Say in one line that it's stopped, and nothing more about it.",
                            (None, NowKind::Connected) => "It was connected already, so nothing was stopped: it stays on.",
                            (None, _) => "Desk is checking its key and turning it on now, so it wasn't stopped: call connections.wait.",
                        };
                        json!({ "now": v.now.title, "next": next })
                    })
            }),
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
            "connections.keep" if input.get("value").and_then(Value::as_str).is_some_and(is_marker) => {
                need().and_then(|id| {
                    let kept = setups.keep_given(
                        &caller,
                        &id,
                        &text(&input, "field").unwrap_or_default(),
                        input.get("value").and_then(Value::as_str).unwrap_or_default(),
                    )?;
                    setups.finish_when_ready(&caller, &kept);
                    Ok(json!(kept))
                })
            }
            "connections.keep" => match need().and_then(|id| {
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
                    .map(|_| id)
            }) {
                Err(e) => Err(e),
                // FreeAgent's new address added: approving is next.
                Ok(id) => match setups.advance(&caller, &id).await {
                    Err(e) => Err(e),
                    Ok(()) => setups
                        .with(&caller, &id, |s| Ok(s.view()))
                        .map(|v| json!({ "steps": v.steps, "now": v.now.title })),
                },
            },
            "connections.test" => match need() {
                Err(e) => Err(e),
                // Desk is already checking it, or has: not twice.
                Ok(id)
                    if setups
                        .with(&caller, &id, |s| Ok(s.finishing || s.connected.is_some()))
                        .unwrap_or(false) =>
                {
                    Ok(
                        json!({ "next": "Desk is checking it and turning it on itself: call connections.wait." }),
                    )
                }
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

/// Desk's link to setup `id`'s card: a click brings it into view, in the
/// chat that asked for it or at the top of whatever Desk shows.
pub fn card_link(id: &str) -> String {
    format!("desk://setup/{id}")
}

/// The one line the agent says before it waits, so the person knows the
/// card is there, wherever they are looking.
fn say_card(view: &SetupView) -> String {
    format!(
        "First tell the person, in this one line and no other words: \"I've put a {} card here: [open it]({})\".",
        view.service_name,
        card_link(&view.id)
    )
}

/// Registers `connections.*` for `service`, with Desk's browser they set
/// connections up in and its documents folder ([`crate::desk_browser::equip`]).
pub fn register(service: &crate::serve::AgentService) {
    for tool in SetupTool::all(service.agent_id.as_str()) {
        if !service.tools.has(tool.name) {
            service.tools.register(Arc::new(tool));
        }
    }
    crate::desk_browser::equip(service);
}

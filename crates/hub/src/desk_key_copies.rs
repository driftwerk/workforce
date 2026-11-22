//! A person's own keys on this hub, copied by their paired Desk into Desk's
//! vault on their computer (Settings → Connections: "Patchwork has 4 of
//! your connections. Set them up here").
//!
//! - `GET /api/me/desk-key-copies`: the person's connections here that
//!   Desk can set up: each one's service, key, name, site and account, and
//!   when it last changed. Never a secret, nor any part of one.
//! - `POST /api/me/desk-key-copies/take` `{connections: [{connector, key}]}`:
//!   those connections' fields, values and all, for Desk's native side to
//!   check and keep. Desk's page is never given this route
//!   (`hub_io::page_refused` in Desk).
//!
//! Only the tiers that are the person's own are read, each under their
//! `users/<id>/`:
//!
//! - **personal** (`global_per_user`): `users/<id>/` in this hub's vault;
//! - **through an agent** (`agent_per_user`): `users/<id>/` in the own vault
//!   of each agent the person can use here (the `/api/agents` roster's rule,
//!   [`AgentViewer::can_see`], judged as the person's paired device and
//!   never as an admin). An agent's vault that is this hub's vault (one
//!   configured on the same file) is skipped: its `users/<id>/` is the
//!   personal tier.
//!
//! The shared tiers (`global`, everyone on the hub; `agent_specific`,
//! everyone using that agent) and every other person's `users/<…>/` are
//! never opened. Within a tier, the paths are the ones Connections' catalog
//! names, flat (`users/<id>/skills/jira/api_token`, the `default`
//! connection, as `/api/me/vault` keeps them) or as an instance
//! (`users/<id>/skills/jira/workspaces/<key>/…`), and of a connection only
//! the fields its catalog entry names are handed over (an extra kept beside
//! them, such as GitHub's `app_private_key`, stays here). Slack's bot token
//! is shared by nature and Google's tokens are this hub's OAuth client's,
//! so neither is copied (Google's presence is said, as `signInOnly`, so
//! Desk can say to sign in there). Nor is a GitHub token from a sign-in
//! (GitHub's device flow, on an OAuth app: `gho_…`, `ghu_…`): revoking that
//! sign-in here would not revoke a copy on Desk. A personal access token is
//! the person's own, and is copied.
//!
//! A connection through an agent is listed only when Desk asks for them
//! (`?with=agents`): a Desk from before took every pick from the personal
//! tier, and would have been handed a different key than it offered. A pick
//! names its agent (`agent`, absent for the personal tier) and is looked up
//! in that tier alone, among the agents the person can use as of the take:
//! an agent they can't use, or that isn't here, is refused in the same
//! words, and nothing is handed over.
//!
//! Only a paired desktop device's token is taken ([`paired_device`]): the
//! person it is linked to is the keys' holder. A sign-in (even an
//! admin's), the hub's own token, a remote-MCP, Sees or boss-link token, an
//! agent and a phone are each refused, so no tool call reaches either
//! route. `[desk] allow_personal_key_export` (on by default) turns both
//! off. Each connection handed over leaves one `audit_log` row and one
//! `sentinel.audit` line ("Your Jira key was handed over to Desk on
//! <device>"), never a value: whether Desk's check then passed is said by
//! Desk's own hub, in its row. The copy on Desk is its own: removing either
//! leaves the other.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use workforce_common::settings::VaultSource;
use workforce_identity::types::IdentityStatus;

use crate::api_handlers::remote_mcp_tokens::paired_device;
use crate::desk_connections::catalog::{Connector, SignIn, CATALOG, DEFAULT_INSTANCE};
use crate::desk_connections::is_key;
use crate::gateway::{AgentViewer, CallerContext, GatewayState};
use crate::rate_limit::IdentityActionLimiter;
use crate::vault::Vault;

pub const LIST_PATH: &str = "/api/me/desk-key-copies";
pub const TAKE_PATH: &str = "/api/me/desk-key-copies/take";

/// The most connections one take asks for.
pub const MAX_TAKE: usize = 12;

/// Takes a person's Desks make, at most, a minute.
static TAKES: LazyLock<IdentityActionLimiter> =
    LazyLock::new(|| IdentityActionLimiter::new(10, Duration::from_secs(60)));
/// Lists a person's Desks make, at most, a minute.
static LISTS: LazyLock<IdentityActionLimiter> =
    LazyLock::new(|| IdentityActionLimiter::new(60, Duration::from_secs(60)));

/// The routes, on `state`.
pub fn router(state: Arc<GatewayState>) -> Router {
    Router::new()
        .route(LIST_PATH, get(list))
        .route(TAKE_PATH, post(take))
        .with_state(state)
}

/// An agent whose own vault holds some of the person's keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOf {
    pub id: String,
    /// Its name, else its id.
    pub name: String,
}

/// A tier of the person's own: the vault it is in, and the agent it is
/// through (`None`: the personal tier). Every read is of
/// `users/<person>/…` in `vault`, never a bare path.
#[derive(Clone)]
pub struct Source {
    pub vault: Arc<Vault>,
    pub agent: Option<AgentOf>,
}

impl Source {
    pub fn personal(vault: Arc<Vault>) -> Self {
        Self { vault, agent: None }
    }

    fn tier(&self) -> VaultSource {
        match self.agent {
            None => VaultSource::GlobalPerUser,
            Some(_) => VaultSource::AgentPerUser,
        }
    }

    /// `path` of the person's, in this tier.
    fn get(&self, person: &Uuid, path: &str) -> Option<String> {
        self.vault.get(&own(person, path))
    }

    fn agent_id(&self) -> Option<&str> {
        self.agent.as_ref().map(|a| a.id.as_str())
    }
}

/// One connection of the person's, as listed: never a value.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Listed {
    /// Connections' connector id: `jira`.
    pub connector: &'static str,
    /// Its name: `Jira`.
    pub service: &'static str,
    /// The connection's key here: `default`, `acme`.
    pub key: String,
    /// The name the person gave it, else the service's.
    pub name: String,
    /// Where it is, for a service with a site.
    pub site: Option<String>,
    /// The account it is, where a field says so in plain (an email).
    pub account: Option<String>,
    /// When any of its fields last changed.
    pub updated_at: Option<DateTime<Utc>>,
    /// The tier it is kept in: `global_per_user` (personal) or
    /// `agent_per_user` (through `agent`).
    pub tier: &'static str,
    /// The agent it is kept through, for `agent_per_user`.
    pub agent: Option<String>,
    /// That agent's name.
    pub agent_name: Option<String>,
}

/// One connection of the person's, taken: its fields, values and all.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Taken {
    #[serde(flatten)]
    listed: Listed,
    fields: BTreeMap<&'static str, String>,
}

#[derive(Deserialize)]
struct TakeBody {
    connections: Vec<Pick>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
struct Pick {
    connector: String,
    key: String,
    /// The agent it is kept through; absent for the personal tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
}

#[derive(Deserialize)]
struct ListQuery {
    /// `agents`: list the connections through agents too.
    #[serde(default)]
    with: Option<String>,
}

fn refused(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

/// A connector whose keys a person may copy: in the catalog, kept per
/// person, and connected by its fields (not Slack's shared bot token, nor
/// Google's sign-in on this hub's own client).
pub fn copyable(id: &str) -> Option<&'static Connector> {
    CATALOG
        .iter()
        .find(|c| c.id == id)
        .filter(|c| !c.shared && c.sign_in != SignIn::App && !c.fields.is_empty())
}

/// How the tokens GitHub issues to an app on a sign-in begin: an OAuth
/// app's (`gho_`, the device flow's), a GitHub App's on behalf of a person
/// (`ghu_`) and its refresh token (`ghr_`). A personal access token is
/// `ghp_` or `github_pat_`.
const GITHUB_SIGN_IN_TOKENS: &[&str] = &["gho_", "ghu_", "ghr_"];

/// Whether connection `key` of `c`, kept in `shape` in `src`, holds a token
/// from a sign-in rather than one the person made: never copied, as
/// revoking the sign-in here would leave the copy working.
fn from_sign_in(src: &Source, c: &Connector, person: &Uuid, key: &str, shape: Shape) -> bool {
    c.sign_in == SignIn::Github
        && c.fields.iter().filter(|f| f.secret).any(|f| {
            src.get(person, &field_path(c, f, key, shape))
                .is_some_and(|v| {
                    let v = v.trim_start();
                    GITHUB_SIGN_IN_TOKENS.iter().any(|p| v.starts_with(p))
                })
        })
}

/// How connection `key` of `c` is kept for `person` in `src`, if it is
/// connected and Desk may copy it (not a sign-in's token).
fn copyable_shape(src: &Source, c: &Connector, person: &Uuid, key: &str) -> Option<Shape> {
    shape_of(src, c, person, key).filter(|&shape| !from_sign_in(src, c, person, key, shape))
}

/// Whether this hub's admin lets a person's Desk copy their keys, now.
fn export_allowed(state: &GatewayState) -> bool {
    state.desk_key_export.on()
}

/// The paired desktop device asking, and the active person it is linked
/// to, when this hub lets Desk copy keys; else the refusal.
async fn caller(
    state: &GatewayState,
    headers: &HeaderMap,
) -> Result<(workforce_pairing::ApprovedDevice, Uuid), Response> {
    let (device, person) = match paired_device(state, headers).await {
        Ok(d) => d,
        Err((StatusCode::FORBIDDEN, _)) => {
            return Err(refused(
                StatusCode::FORBIDDEN,
                "Only your own Desk, paired with this hub, copies your keys",
            ))
        }
        Err(r) => return Err(r.into_response()),
    };
    let active = state
        .identity_store
        .as_ref()
        .and_then(|s| s.get_by_id(person).ok().flatten())
        .is_some_and(|i| i.status == IdentityStatus::Active);
    if !active {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "You aren't active on this hub",
        ));
    }
    if !export_allowed(state) {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "This hub's admin has turned off copying your keys to Desk",
        ));
    }
    Ok((device, person))
}

/// The person's own tiers on this hub, the personal one first and then each
/// agent's they can use, by agent id; `None` when this hub keeps no vault.
/// With `agents` false, only the personal tier.
fn sources_of(
    state: &GatewayState,
    device: &workforce_pairing::ApprovedDevice,
    person: &Uuid,
    agents: bool,
) -> Option<Vec<Source>> {
    let global = state.vault.as_ref()?;
    let mut out = vec![Source::personal(Arc::clone(global))];
    if !agents {
        return Some(out);
    }
    // Judged as `/api/agents` judges this device: the person, never an
    // admin, so an admin's Desk copies only from agents they can use.
    let viewer = AgentViewer::new(
        state,
        CallerContext {
            identity_id: Some(person.to_string()),
            device_id: Some(device.id.clone()),
            is_admin: false,
            scopes: device.scopes.clone(),
            auth_failure: None,
        },
    );
    let mut ids: Vec<String> = state
        .registry
        .agent_ids()
        .into_iter()
        .map(|a| a.as_str().to_string())
        .collect();
    ids.sort();
    for id in ids {
        if !viewer.can_see(&id) {
            continue;
        }
        let Some(service) = state.registry.get(&id) else {
            continue;
        };
        let Some(vault) = service.vault.clone() else {
            continue;
        };
        // An agent on this hub's own vault file: its `users/<id>/` is the
        // personal tier, already read.
        if Arc::ptr_eq(&vault, global) {
            continue;
        }
        let name = service
            .display_name()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| id.clone());
        out.push(Source {
            vault,
            agent: Some(AgentOf { id, name }),
        });
    }
    Some(out)
}

fn limited(limiter: &IdentityActionLimiter, person: &Uuid) -> Option<Response> {
    limiter
        .check_and_record(&person.to_string())
        .err()
        .map(|retry| {
            let mut r = refused(
                StatusCode::TOO_MANY_REQUESTS,
                "Desk asked for your keys too often just now; try again in a minute",
            );
            if let Ok(v) = HeaderValue::from_str(&retry.to_string()) {
                r.headers_mut().insert(header::RETRY_AFTER, v);
            }
            r
        })
}

fn no_store(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// The person's own part of a vault: `path` under `users/<person>/`.
fn own(person: &Uuid, path: &str) -> String {
    format!("users/{person}/{path}")
}

/// Where connection `key` of `c` keeps `sub`-shaped paths: the instance,
/// or (for `default` with no instance of that name) the flat paths a
/// connection was kept at before services held several.
#[derive(Clone, Copy)]
enum Shape {
    Instance,
    Flat,
}

/// How connection `key` of `c` is kept in `src` for `person`, if it is
/// connected (its primary field holds a value).
fn shape_of(src: &Source, c: &Connector, person: &Uuid, key: &str) -> Option<Shape> {
    let instance = c.prefix.is_some() && is_key(key);
    if instance && src.get(person, &c.primary_at(key)).is_some() {
        return Some(Shape::Instance);
    }
    (key == DEFAULT_INSTANCE && src.get(person, c.primary).is_some()).then_some(Shape::Flat)
}

/// Where `field` of connection `key` is, in `shape`.
fn field_path(
    c: &Connector,
    f: &crate::desk_connections::catalog::Field,
    key: &str,
    shape: Shape,
) -> String {
    match shape {
        Shape::Instance => c.field_at(f, key),
        Shape::Flat => f.path.to_string(),
    }
}

/// Connection `key` of `c` in `src` as listed, from its kept fields (values
/// unread but the plain ones shown).
fn listed(src: &Source, c: &'static Connector, person: &Uuid, key: &str, shape: Shape) -> Listed {
    let plain = |name: &str| {
        c.fields
            .iter()
            .find(|f| f.name == name && !f.secret)
            .and_then(|f| src.get(person, &field_path(c, f, key, shape)))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let label = match shape {
        Shape::Instance => c
            .label_at(key)
            .and_then(|p| src.get(person, &p))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        Shape::Flat => None,
    };
    let updated_at = c
        .fields
        .iter()
        .filter_map(|f| {
            src.vault
                .get_entry(&own(person, &field_path(c, f, key, shape)))
        })
        .map(|e| e.updated_at)
        .max();
    Listed {
        connector: c.id,
        service: c.name,
        key: key.to_string(),
        name: label.unwrap_or_else(|| match key {
            DEFAULT_INSTANCE => c.name.to_string(),
            other => other.to_string(),
        }),
        site: c.site.and_then(plain),
        account: plain("email").or_else(|| plain("username")),
        updated_at,
        tier: src.tier().label(),
        agent: src.agent.as_ref().map(|a| a.id.clone()),
        agent_name: src.agent.as_ref().map(|a| a.name.clone()),
    }
}

/// Where a connection is, when that says which workspace it is: its site
/// and account, or, with neither, the name the person gave it (a Linear
/// workspace's). `None` for one named only by its service (an unnamed
/// GitHub or Linear key): nothing then says two are the same workspace.
fn place(l: &Listed) -> Option<(Option<String>, Option<String>, Option<String>)> {
    let n = |s: &str| {
        s.trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_lowercase()
    };
    let site = l.site.as_deref().map(n);
    let account = l.account.as_deref().map(n);
    if site.is_none() && account.is_none() {
        return (l.name != l.service).then(|| (None, None, Some(n(&l.name))));
    }
    Some((site, account, None))
}

/// The secret fields' values of connection `key` of `c` in `src`, compared
/// only with another's here, never handed on.
fn secrets_of(
    src: &Source,
    c: &Connector,
    person: &Uuid,
    key: &str,
    shape: Shape,
) -> Vec<Option<String>> {
    c.fields
        .iter()
        .filter(|f| f.secret)
        .map(|f| src.get(person, &field_path(c, f, key, shape)))
        .collect()
}

/// The keys of `c` the person keeps in `src`: each instance's, and
/// `default` (an instance, or the flat paths).
fn keys_in(src: &Source, c: &Connector, person: &Uuid) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    if let Some(prefix) = c.prefix {
        let at = own(person, prefix);
        for path in src.vault.list(Some(&at)) {
            let Some((key, _)) = path.strip_prefix(&at).and_then(|r| r.split_once('/')) else {
                continue;
            };
            if is_key(key) && !keys.iter().any(|k| k == key) {
                keys.push(key.to_string());
            }
        }
    }
    if !keys.iter().any(|k| k == DEFAULT_INSTANCE) {
        keys.push(DEFAULT_INSTANCE.to_string());
    }
    keys.sort();
    keys
}

/// Every connection of the person's own in `sources` that Desk can set up,
/// by service in the catalog's order, then tier (as `sources` are ordered:
/// the personal one first, then each agent's by id), then key.
///
/// One connection kept in two tiers is listed once, from the first tier
/// holding it: the personal one over an agent's, and an agent's over a
/// later agent's (the rule asked for: the person's own key for all their
/// agents is the one Desk sets up for all of theirs; at run time an agent's
/// own key wins over it, and stays copyable by name). Two are one
/// connection when the same service either holds the same secrets, or sits
/// at the same [`place`] (the same site and account, or the same name the
/// person gave it). An unnamed key at no site, in two tiers, with different
/// secrets, is two connections, each offered.
pub fn connections_of(sources: &[Source], person: &Uuid) -> Vec<Listed> {
    let mut out: Vec<Listed> = Vec::new();
    for c in CATALOG.iter().filter(|c| copyable(c.id).is_some()) {
        // This service's listed so far, and their secrets (never output).
        let mut seen: Vec<(Listed, Vec<Option<String>>)> = Vec::new();
        for src in sources {
            for key in keys_in(src, c, person) {
                let Some(shape) = copyable_shape(src, c, person, &key) else {
                    continue;
                };
                let l = listed(src, c, person, &key, shape);
                let secrets = secrets_of(src, c, person, &key, shape);
                // Only another tier's copy shadows it: two connections of one
                // tier are each their own.
                let shadowed = seen.iter().any(|(o, their)| {
                    o.agent != l.agent
                        && (*their == secrets || place(o).is_some_and(|p| Some(p) == place(&l)))
                });
                if !shadowed {
                    seen.push((l, secrets));
                }
            }
        }
        out.extend(seen.into_iter().map(|(l, _)| l));
    }
    out
}

/// Whether the person holds a Google sign-in in any of `sources`: said, as
/// it isn't copied, so Desk can say to sign in there instead.
fn signed_in_to_google(sources: &[Source], person: &Uuid) -> bool {
    let Some(google) = CATALOG.iter().find(|c| c.id == "google") else {
        return false;
    };
    sources.iter().any(|src| {
        src.get(person, google.primary).is_some()
            || google.prefix.is_some_and(|prefix| {
                let at = own(person, prefix);
                src.vault
                    .list(Some(&at))
                    .iter()
                    .any(|p| p.ends_with(&format!("/{}", google.primary_sub)))
            })
    })
}

/// Connection `key` of `c` in `src`, its fields and all, if the person
/// holds it there.
fn take_one(src: &Source, c: &'static Connector, person: &Uuid, key: &str) -> Option<Taken> {
    let shape = copyable_shape(src, c, person, key)?;
    let fields = c
        .fields
        .iter()
        .filter_map(|f| {
            src.get(person, &field_path(c, f, key, shape))
                .map(|v| (f.name, v))
        })
        .collect();
    Some(Taken {
        listed: listed(src, c, person, key, shape),
        fields,
    })
}

fn no_vault() -> Response {
    refused(
        StatusCode::SERVICE_UNAVAILABLE,
        "This hub keeps no vault, so it holds no keys of yours",
    )
}

/// `GET /api/me/desk-key-copies[?with=agents]`.
async fn list(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    let (device, person) = match caller(&state, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Some(r) = limited(&LISTS, &person) {
        return r;
    }
    let agents = query
        .with
        .is_some_and(|w| w.split(',').any(|w| w.trim() == "agents"));
    let Some(sources) = sources_of(&state, &device, &person, agents) else {
        return no_vault();
    };
    let sign_in_only: Vec<_> = signed_in_to_google(&sources, &person)
        .then(|| json!({ "connector": "google", "service": "Google" }))
        .into_iter()
        .collect();
    no_store(
        Json(json!({
            "device": device.device_name,
            "connections": connections_of(&sources, &person),
            "signInOnly": sign_in_only,
        }))
        .into_response(),
    )
}

/// `POST /api/me/desk-key-copies/take`.
async fn take(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    body: Option<Json<TakeBody>>,
) -> Response {
    let (device, person) = match caller(&state, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Some(Json(body)) = body else {
        return refused(StatusCode::BAD_REQUEST, "Say which connections to copy");
    };
    if body.connections.is_empty() || body.connections.len() > MAX_TAKE {
        return refused(
            StatusCode::BAD_REQUEST,
            &format!("Copy between 1 and {MAX_TAKE} connections at a time"),
        );
    }
    let mut picks: Vec<(&'static Connector, &Pick)> = Vec::new();
    for pick in &body.connections {
        let Some(c) = copyable(&pick.connector) else {
            return refused(
                StatusCode::BAD_REQUEST,
                "Only your own keys of a service Desk connects by its keys are copied",
            );
        };
        if !(pick.key == DEFAULT_INSTANCE || (c.prefix.is_some() && is_key(&pick.key))) {
            return refused(StatusCode::BAD_REQUEST, "That isn't a connection's key");
        }
        if !picks.iter().any(|(_, p)| *p == pick) {
            picks.push((c, pick));
        }
    }
    if let Some(r) = limited(&TAKES, &person) {
        return r;
    }
    // Judged again now, not taken from a list Desk was given: an agent the
    // person can no longer use hands over nothing.
    let Some(sources) = sources_of(&state, &device, &person, true) else {
        return no_vault();
    };
    let mut asked: Vec<(&'static Connector, &Pick, &Source)> = Vec::new();
    for (c, pick) in picks {
        // The pick's own tier alone: never another tier holding the same
        // key, so what is handed over is what was offered.
        let Some(src) = sources
            .iter()
            .find(|s| s.agent_id() == pick.agent.as_deref())
        else {
            return refused(
                StatusCode::FORBIDDEN,
                "That isn't an agent of yours on this hub, so nothing was handed over",
            );
        };
        asked.push((c, pick, src));
    }
    // Never listed, so asked for only by name: nothing is handed over.
    let signed_in = asked.iter().any(|(c, pick, src)| {
        shape_of(src, c, &person, &pick.key)
            .is_some_and(|shape| from_sign_in(src, c, &person, &pick.key, shape))
    });
    if signed_in {
        return refused(
            StatusCode::BAD_REQUEST,
            "A GitHub sign-in isn't copied: signing it out here wouldn't sign out Desk's copy. \
             Sign in to GitHub on Desk, or connect it there with a personal access token",
        );
    }
    let mut copies = Vec::new();
    let mut missing = Vec::new();
    for (c, pick, src) in asked {
        match take_one(src, c, &person, &pick.key) {
            Some(taken) => {
                audit(&state, &person, &device, &taken.listed);
                copies.push(taken);
            }
            None => missing.push(pick.clone()),
        }
    }
    no_store(Json(json!({ "copies": copies, "missing": missing })).into_response())
}

/// "Your Jira key (Acme) was handed over to Desk on Sarah's Mac", or
/// "Your Linear key (driftwerk), through Allan, was handed over…":
/// handed over, as this hub knows no more; Desk's own hub says whether its
/// check passed and it was kept.
pub fn summary(device: &str, l: &Listed) -> String {
    let device = device.trim();
    let device = if device.is_empty() {
        "a computer"
    } else {
        device
    };
    let of = if l.name == l.service {
        String::new()
    } else {
        format!(" ({})", l.name)
    };
    let through = match (&l.agent_name, &l.agent) {
        (Some(name), _) | (None, Some(name)) => format!(", through {name},"),
        (None, None) => String::new(),
    };
    format!(
        "Your {} key{of}{through} was handed over to Desk on {device}",
        l.service
    )
}

/// One `sentinel.audit` line and one `audit_log` row for connection `l`
/// handed over to `device`, naming its tier and agent: never a value.
fn audit(
    state: &GatewayState,
    person: &Uuid,
    device: &workforce_pairing::ApprovedDevice,
    l: &Listed,
) {
    let said = summary(&device.device_name, l);
    tracing::info!(
        target: "sentinel.audit",
        identity_id = %person,
        device = %device.id,
        connector = %l.connector,
        connection = %l.key,
        tier = %l.tier,
        agent = l.agent.as_deref().unwrap_or("-"),
        "desk.key_copy: {said}"
    );
    let Some(telemetry) = state.telemetry_store.as_ref() else {
        return;
    };
    let detail = json!({
        "summary": said,
        "identity_id": person,
        "device_id": device.id,
        "device": device.device_name,
        "connector": l.connector,
        "connection": l.key,
        "tier": l.tier,
        "agent": l.agent,
    })
    .to_string();
    let target = match &l.agent {
        Some(agent) => format!("desk_key_copy:{}/{}@agent:{agent}", l.connector, l.key),
        None => format!("desk_key_copy:{}/{}", l.connector, l.key),
    };
    if let Err(e) = telemetry.record_audit(
        &format!("user:{person}"),
        "desk.key_copy",
        &target,
        Some(&detail),
        workforce_common::admin::AuditOutcome::Success,
    ) {
        tracing::warn!(error = %e, "a Desk key copy's audit row wasn't written");
    }
}

#[cfg(test)]
#[path = "desk_key_copies_tests.rs"]
mod tests;

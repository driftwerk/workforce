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
//! Only the person's own **personal** tier is read: `users/<id>/` in this
//! hub's vault (GlobalPerUser), at the paths Connections' catalog names,
//! flat (`users/<id>/skills/jira/api_token`, the `default` connection, as
//! `/api/me/vault` keeps them) or as an instance
//! (`users/<id>/skills/jira/workspaces/<key>/…`). The shared tier, an
//! agent's vault (which keeps its own `users/<id>/` too) and every path
//! outside the catalog are never opened. Slack's bot token is shared by
//! nature and Google's tokens are this hub's OAuth client's, so neither is
//! copied.
//!
//! Only a paired desktop device's token is taken ([`paired_device`]): the
//! person it is linked to is the keys' holder. A sign-in (even an
//! admin's), the hub's own token, a remote-MCP, Sees or boss-link token, an
//! agent and a phone are each refused, so no tool call reaches either
//! route. `[desk] allow_personal_key_export` (on by default) turns both
//! off. Each copied connection leaves one `audit_log` row and one
//! `sentinel.audit` line ("Desk on <device> copied your Jira key"), never a
//! value. The copy on Desk is its own: removing either leaves the other.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use workforce_identity::types::IdentityStatus;

use crate::api_handlers::remote_mcp_tokens::paired_device;
use crate::desk_connections::catalog::{Connector, SignIn, CATALOG, DEFAULT_INSTANCE};
use crate::desk_connections::is_key;
use crate::gateway::GatewayState;
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
        .filter(|c| !c.shared && c.sign_in != SignIn::Google && !c.fields.is_empty())
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

/// The person's own tier: `path` under `users/<person>/`.
fn personal(person: &Uuid, path: &str) -> String {
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

/// How connection `key` of `c` is kept in `vault` for `person`, if it is
/// connected (its primary field holds a value).
fn shape_of(vault: &Vault, c: &Connector, person: &Uuid, key: &str) -> Option<Shape> {
    let instance = c.prefix.is_some() && is_key(key);
    if instance && vault.get(&personal(person, &c.primary_at(key))).is_some() {
        return Some(Shape::Instance);
    }
    (key == DEFAULT_INSTANCE && vault.get(&personal(person, c.primary)).is_some())
        .then_some(Shape::Flat)
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

/// Connection `key` of `c` as listed, from its kept fields (values unread
/// but the plain ones shown).
fn listed(vault: &Vault, c: &'static Connector, person: &Uuid, key: &str, shape: Shape) -> Listed {
    let plain = |name: &str| {
        c.fields
            .iter()
            .find(|f| f.name == name && !f.secret)
            .and_then(|f| vault.get(&personal(person, &field_path(c, f, key, shape))))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let label = match shape {
        Shape::Instance => c
            .label_at(key)
            .and_then(|p| vault.get(&personal(person, &p)))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        Shape::Flat => None,
    };
    let updated_at = c
        .fields
        .iter()
        .filter_map(|f| vault.get_entry(&personal(person, &field_path(c, f, key, shape))))
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
    }
}

/// Every connection of the person's own in `vault` that Desk can set up,
/// by service in the catalog's order, then key.
pub fn connections_of(vault: &Vault, person: &Uuid) -> Vec<Listed> {
    let mut out = Vec::new();
    for c in CATALOG.iter().filter(|c| copyable(c.id).is_some()) {
        let mut keys: Vec<String> = Vec::new();
        if let Some(prefix) = c.prefix {
            let at = personal(person, prefix);
            for path in vault.list(Some(&at)) {
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
        for key in keys {
            if let Some(shape) = shape_of(vault, c, person, &key) {
                out.push(listed(vault, c, person, &key, shape));
            }
        }
    }
    out
}

/// Connection `key` of `c`, its fields and all, if the person holds it.
fn take_one(vault: &Vault, c: &'static Connector, person: &Uuid, key: &str) -> Option<Taken> {
    let shape = shape_of(vault, c, person, key)?;
    let fields = c
        .fields
        .iter()
        .filter_map(|f| {
            vault
                .get(&personal(person, &field_path(c, f, key, shape)))
                .map(|v| (f.name, v))
        })
        .collect();
    Some(Taken {
        listed: listed(vault, c, person, key, shape),
        fields,
    })
}

fn no_vault() -> Response {
    refused(
        StatusCode::SERVICE_UNAVAILABLE,
        "This hub keeps no vault, so it holds no keys of yours",
    )
}

/// `GET /api/me/desk-key-copies`.
async fn list(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    let (device, person) = match caller(&state, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Some(r) = limited(&LISTS, &person) {
        return r;
    }
    let Some(vault) = state.vault.as_ref() else {
        return no_vault();
    };
    no_store(
        Json(json!({
            "device": device.device_name,
            "connections": connections_of(vault, &person),
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
    let Some(vault) = state.vault.as_ref() else {
        return no_vault();
    };
    let mut copies = Vec::new();
    let mut missing = Vec::new();
    for (c, pick) in picks {
        match take_one(vault, c, &person, &pick.key) {
            Some(taken) => {
                audit(&state, &person, &device, &taken.listed);
                copies.push(taken);
            }
            None => missing.push(pick.clone()),
        }
    }
    no_store(Json(json!({ "copies": copies, "missing": missing })).into_response())
}

/// "Desk on Sarah's Mac copied your Jira key (Acme)".
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
    format!("Desk on {device} copied your {} key{of}", l.service)
}

/// One `sentinel.audit` line and one `audit_log` row for connection `l`
/// copied by `device`: never a value.
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
    })
    .to_string();
    if let Err(e) = telemetry.record_audit(
        &format!("user:{person}"),
        "desk.key_copy",
        &format!("desk_key_copy:{}/{}", l.connector, l.key),
        Some(&detail),
        workforce_common::admin::AuditOutcome::Success,
    ) {
        tracing::warn!(error = %e, "a Desk key copy's audit row wasn't written");
    }
}

#[cfg(test)]
#[path = "desk_key_copies_tests.rs"]
mod tests;

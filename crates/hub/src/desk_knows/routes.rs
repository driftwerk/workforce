//! The routes a person's Desk shares their work with an agent here by.
//!
//! - `POST /api/me/desk-knows-tokens` `{agent_id, engine: {id, name,
//!   public_key}}` (**the paired device's own token alone**): mints a `wfk_`
//!   token for that agent, replacing the one this device held for it.
//!   Answers it once.
//! - `DELETE /api/me/desk-knows-tokens/{id}` (the device): revokes one it
//!   minted. Taken even while `[desk_knows]` is off.
//! - `POST /api/desk-knows/push` `{items: [{kind, ref, session?, group?, at,
//!   body}]}` (**the `wfk_` token alone**): keeps them for the person, the
//!   agent and the device. At most [`MAX_ITEMS`] items and [`MAX_BODY`]
//!   bytes.
//! - `POST /api/desk-knows/forget` `{items?: [[kind, ref]], sessions?,
//!   kinds?, all?}` (the token): deletes what it covers. Taken even while
//!   `[desk_knows]` is off or the agent no longer accepts, so what was
//!   shared can always be taken back.
//! - `GET /api/desk-knows/status` (the token): what is kept, by kind.
//!
//! Each push and forget is one `sentinel.audit` line and audit row, with
//! counts and never content.

// A refusal is the route's own answer, returned as it is.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::warn;
use uuid::Uuid;

use workforce_common::settings::{AgentWorkspaceConfig, DeskKnowsConfig};
use workforce_identity::remote_mcp_token::{
    self, DeskLink, RemoteMcpToken, TokenScope, DESK_KNOWS_TOKEN_PREFIX, DEVICE_SCOPE_PREFIX,
};
use workforce_identity::types::IdentityStatus;

use super::store::{Forget, Item, KnowsStore, Owner};
use crate::api_handlers::my_settings::enforce_action_rate_limit;
use crate::api_handlers::remote_mcp_tokens::paired_device;
use crate::boss_links::engine_key::EnginePublicKey;
use crate::gateway::GatewayState;

/// The most items one push may carry.
pub const MAX_ITEMS: usize = 256;
/// The most bytes one push may be.
pub const MAX_BODY: usize = 1024 * 1024;
/// The longest one item's text may be, in characters.
pub const MAX_TEXT: usize = 80_000;
const MAX_REF: usize = 200;
const MAX_NAME: usize = 200;
const AGENT_ID_MAX: usize = 128;
/// The kinds a Desk shares.
pub const KINDS: [&str; 5] = ["account", "decision", "need", "board", "transcript"];

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

fn config(state: &GatewayState) -> DeskKnowsConfig {
    state
        .registry
        .default_agent()
        .map(|s| s.config.desk_knows.clone())
        .unwrap_or_default()
}

fn store() -> Result<Arc<dyn KnowsStore>, Response> {
    super::installed().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "This hub keeps no Desk work: it has no Postgres.",
        )
    })
}

fn disabled() -> Response {
    err(
        StatusCode::FORBIDDEN,
        "This hub's admin has turned off sharing Desk work with its agents.",
    )
}

fn declines() -> Response {
    err(
        StatusCode::FORBIDDEN,
        "That agent doesn't take Desk work here.",
    )
}

/// The agent `agent_id` names when `person` may use it and it accepts Desk
/// work (`[desk_knows] accept`, read each time): its canonical id.
fn knows_agent(state: &GatewayState, person: Uuid, agent_id: &str) -> Result<String, Response> {
    let gone = || {
        err(
            StatusCode::NOT_FOUND,
            "That agent isn't one you can use here.",
        )
    };
    if agent_id.trim().is_empty() || agent_id.chars().count() > AGENT_ID_MAX {
        return Err(gone());
    }
    let service =
        crate::remote_mcp::access::reachable(state, &person.to_string(), Some(agent_id.trim()))
            .map_err(|_| gone())?;
    let live: Vec<String> = state
        .registry
        .agent_ids()
        .iter()
        .map(|a| a.to_string())
        .collect();
    let id = crate::agent_registry::canonical_agent_id(agent_id.trim(), &live)
        .map(str::to_string)
        .ok_or_else(gone)?;
    let accepts = AgentWorkspaceConfig::load(&service.workspace)
        .ok()
        .flatten()
        .is_none_or(|c| c.accepts_desk_knows());
    if !accepts {
        return Err(declines());
    }
    Ok(id)
}

fn tokens(
    state: &GatewayState,
) -> Result<&dyn workforce_identity::remote_mcp_token::RemoteMcpTokenStore, Response> {
    state
        .remote_mcp_tokens
        .as_deref()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "This hub keeps no tokens."))
}

#[derive(Deserialize)]
pub struct EngineBody {
    id: String,
    name: String,
    public_key: String,
}

#[derive(Deserialize)]
pub struct MintBody {
    agent_id: String,
    engine: EngineBody,
}

fn plain(text: &str, max: usize) -> Option<&str> {
    let t = text.trim();
    (!t.is_empty() && t.chars().count() <= max && !t.chars().any(char::is_control)).then_some(t)
}

/// `POST /api/me/desk-knows-tokens`.
pub async fn mint(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<MintBody>,
) -> Response {
    let cfg = config(&state);
    if !cfg.enabled {
        return disabled();
    }
    let (device, person) = match paired_device(&state, &headers).await {
        Ok(d) => d,
        Err(r) => return r.into_response(),
    };
    if let Err(r) = enforce_action_rate_limit(&state, &person.to_string()) {
        return r.into_response();
    }
    let active = state
        .identity_store
        .as_ref()
        .and_then(|s| s.get_by_id(person).ok().flatten())
        .is_some_and(|i| i.status == IdentityStatus::Active);
    if !active {
        return err(StatusCode::FORBIDDEN, "You aren't active on this hub.");
    }
    let agent_id = match knows_agent(&state, person, &body.agent_id) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let (Some(engine_id), Some(engine_name)) = (
        plain(&body.engine.id, MAX_NAME),
        plain(&body.engine.name, MAX_NAME),
    ) else {
        return err(StatusCode::BAD_REQUEST, "Say the engine's id and name.");
    };
    let Ok(key) = EnginePublicKey::parse(&body.engine.public_key) else {
        return err(StatusCode::BAD_REQUEST, "That isn't an engine key.");
    };
    let store = match tokens(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let now = Utc::now();
    let actor = person.to_string();
    // One live token per (device, agent): a fresh one replaces it.
    match store.list_tokens_for_identity(person) {
        Ok(held) => {
            for t in held.iter().filter(|t| {
                t.is_live(now)
                    && t.scope() == Some(TokenScope::DeskKnows)
                    && t.minted_by_device() == Some(device.id.as_str())
                    && t.desk_link.as_ref().is_some_and(|l| l.agent_id == agent_id)
            }) {
                if store.revoke_token(t.id, Some(person), now).unwrap_or(false) {
                    crate::boss_links::audit(
                        state.telemetry_store.as_ref(),
                        &actor,
                        "desk_knows.revoke",
                        t,
                        "replaced",
                    );
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "desk knows token store error");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "token store error");
        }
    }
    let minted = remote_mcp_token::mint_as(TokenScope::DeskKnows);
    let record = RemoteMcpToken {
        id: Uuid::new_v4(),
        identity_id: person,
        label: format!("Desk work for {agent_id} from {engine_name}"),
        token_hash: minted.hash,
        scopes: vec![
            TokenScope::DeskKnows.key().to_string(),
            format!("{DEVICE_SCOPE_PREFIX}{}", device.id),
        ],
        created_at: now,
        expires_at: now + Duration::days(i64::from(cfg.ttl_days())),
        last_used_at: None,
        revoked_at: None,
        desk_link: Some(DeskLink {
            link_id: Uuid::new_v4(),
            agent_id: agent_id.clone(),
            group_key: String::new(),
            desk_group_key: String::new(),
            group_name: String::new(),
            engine_id: engine_id.to_string(),
            engine_name: engine_name.to_string(),
            engine_public_key: key.to_text(),
        }),
    };
    if let Err(e) = store.create_token(&record) {
        warn!(error = %e, "desk knows token store error");
        return err(StatusCode::INTERNAL_SERVER_ERROR, "token store error");
    }
    crate::boss_links::audit(
        state.telemetry_store.as_ref(),
        &actor,
        "desk_knows.mint",
        &record,
        "new",
    );
    (
        StatusCode::CREATED,
        Json(json!({
            "id": record.id,
            "token": minted.plaintext,
            "agentId": agent_id,
            "expiresAt": record.expires_at,
        })),
    )
        .into_response()
}

/// `DELETE /api/me/desk-knows-tokens/{id}`: only one this device minted;
/// any other id is "not found", whoever holds it.
pub async fn revoke(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (device, person) = match paired_device(&state, &headers).await {
        Ok(d) => d,
        Err(r) => return r.into_response(),
    };
    let not_found = || err(StatusCode::NOT_FOUND, "No such token.");
    let Ok(id) = id.parse::<Uuid>() else {
        return not_found();
    };
    let store = match tokens(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let held = store
        .list_tokens_for_identity(person)
        .unwrap_or_default()
        .into_iter()
        .find(|t| {
            t.id == id
                && t.scope() == Some(TokenScope::DeskKnows)
                && t.minted_by_device() == Some(device.id.as_str())
        });
    let Some(t) = held else {
        return not_found();
    };
    match store.revoke_token(t.id, Some(person), Utc::now()) {
        Ok(_) => {
            crate::boss_links::audit(
                state.telemetry_store.as_ref(),
                &person.to_string(),
                "desk_knows.revoke",
                &t,
                "device",
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            warn!(error = %e, "desk knows token store error");
            err(StatusCode::INTERNAL_SERVER_ERROR, "token store error")
        }
    }
}

/// A `wfk_` token that is live now: whose and for which agent.
pub struct Live {
    pub token: RemoteMcpToken,
    pub owner: Owner,
}

/// Whether the bearer is a live Desk knows token: this hub minted it, it is
/// unrevoked and unexpired, of exactly this kind with its agent, its device
/// still paired, its person active. With `serving`, also `[desk_knows]`
/// enabled and the agent still the person's and still accepting: a forget
/// is taken without them, so what was shared can always be taken back.
pub fn authenticate(
    state: &GatewayState,
    headers: &HeaderMap,
    serving: bool,
) -> Result<Live, Response> {
    let unknown = || {
        err(
            StatusCode::UNAUTHORIZED,
            "This hub doesn't know that token, or it has ended.",
        )
    };
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .filter(|t| t.starts_with(DESK_KNOWS_TOKEN_PREFIX))
        .ok_or_else(unknown)?;
    let store = tokens(state)?;
    let now = Utc::now();
    let token = match store.get_token_by_hash(&remote_mcp_token::hash_token(presented)) {
        Ok(Some(t)) if t.is_live(now) && t.scope() == Some(TokenScope::DeskKnows) => t,
        Ok(_) => return Err(unknown()),
        Err(e) => {
            warn!(error = %e, "desk knows token lookup failed — failing closed");
            return Err(err(
                StatusCode::SERVICE_UNAVAILABLE,
                "This hub couldn't check that token; try again.",
            ));
        }
    };
    let (Some(link), Some(device)) = (token.desk_link.clone(), token.minted_by_device()) else {
        return Err(unknown());
    };
    let device = device.to_string();
    crate::remote_mcp::auth::device_still_paired(state, &device, &token).map_err(|_| unknown())?;
    let active = state
        .identity_store
        .as_ref()
        .and_then(|s| s.get_by_id(token.identity_id).ok().flatten())
        .is_some_and(|i| i.status == IdentityStatus::Active);
    if !active {
        return Err(err(
            StatusCode::FORBIDDEN,
            "The person this token is for isn't active here.",
        ));
    }
    if serving {
        if !config(state).enabled {
            return Err(disabled());
        }
        match knows_agent(state, token.identity_id, &link.agent_id) {
            Ok(id) if id == link.agent_id => {}
            Ok(_) => return Err(declines()),
            Err(r) => return Err(r),
        }
    }
    Ok(Live {
        owner: Owner {
            person: token.identity_id,
            source: device,
            agent: link.agent_id,
        },
        token,
    })
}

#[derive(Deserialize)]
struct PushItem {
    kind: String,
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    group: Option<String>,
    at: DateTime<Utc>,
    body: Value,
}

#[derive(Deserialize)]
struct PushBody {
    items: Vec<PushItem>,
}

/// One pushed item, checked: a known kind, plain ids, an object body whose
/// text is a string within [`MAX_TEXT`].
fn checked(i: PushItem) -> Result<Item, String> {
    if !KINDS.contains(&i.kind.as_str()) {
        return Err(format!("{:?} isn't a kind Desk shares.", i.kind));
    }
    let reference = plain(&i.reference, MAX_REF).ok_or("Each item needs a short ref.")?;
    let session = match i.session.as_deref() {
        None => None,
        Some(s) => Some(
            plain(s, MAX_REF)
                .ok_or("A session is named by a short id.")?
                .to_string(),
        ),
    };
    let group_name = match i.group.as_deref() {
        None => None,
        Some(g) => Some(
            plain(g, MAX_NAME)
                .ok_or("A group is named in a short line.")?
                .to_string(),
        ),
    };
    let Some(obj) = i.body.as_object() else {
        return Err("Each item's body is an object.".into());
    };
    match obj.get("text") {
        Some(Value::String(t)) if t.chars().count() <= MAX_TEXT => {}
        Some(Value::String(_)) => {
            return Err(format!("An item's text is at most {MAX_TEXT} characters."))
        }
        _ => return Err("Each item's body has its text.".into()),
    }
    Ok(Item {
        kind: i.kind,
        reference: reference.to_string(),
        session,
        group_name,
        at: i.at,
        body: i.body,
    })
}

/// `POST /api/desk-knows/push`.
pub async fn push(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let live = match authenticate(&state, &headers, true) {
        Ok(l) => l,
        Err(r) => return r,
    };
    if body.len() > MAX_BODY {
        return err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "A push is at most 1 MiB: send fewer items.",
        );
    }
    let parsed: PushBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(_) => return err(StatusCode::BAD_REQUEST, "That isn't a push Desk makes."),
    };
    if parsed.items.len() > MAX_ITEMS {
        return err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "A push is at most 256 items.",
        );
    }
    let items = match parsed
        .items
        .into_iter()
        .map(checked)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(i) => i,
        Err(why) => return err(StatusCode::BAD_REQUEST, &why),
    };
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match store.push(&live.owner, &items, Utc::now()).await {
        Ok(kept) => {
            crate::boss_links::audit(
                state.telemetry_store.as_ref(),
                &live.owner.person.to_string(),
                "desk_knows.push",
                &live.token,
                &format!("{kept} items"),
            );
            Json(json!({ "kept": kept })).into_response()
        }
        Err(e) => {
            warn!(error = %format!("{e:#}"), "keeping what Desk shared failed");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "This hub couldn't keep that; Desk will try again.",
            )
        }
    }
}

#[derive(Deserialize, Default)]
struct ForgetBody {
    #[serde(default)]
    items: Vec<(String, String)>,
    #[serde(default)]
    sessions: Vec<String>,
    #[serde(default)]
    kinds: Vec<String>,
    #[serde(default)]
    all: bool,
}

/// `POST /api/desk-knows/forget`.
pub async fn forget(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let live = match authenticate(&state, &headers, false) {
        Ok(l) => l,
        Err(r) => return r,
    };
    if body.len() > 4 * MAX_BODY {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "Forget fewer at once.");
    }
    let Ok(f) = serde_json::from_slice::<ForgetBody>(&body) else {
        return err(StatusCode::BAD_REQUEST, "That isn't a forget Desk makes.");
    };
    let forget = Forget {
        items: f.items,
        sessions: f.sessions,
        kinds: f.kinds,
        all: f.all,
    };
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match store.forget(&live.owner, &forget).await {
        Ok(gone) => {
            crate::boss_links::audit(
                state.telemetry_store.as_ref(),
                &live.owner.person.to_string(),
                "desk_knows.forget",
                &live.token,
                &format!("{gone} items"),
            );
            Json(json!({ "forgotten": gone })).into_response()
        }
        Err(e) => {
            warn!(error = %format!("{e:#}"), "forgetting what Desk shared failed");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "This hub couldn't forget that yet; Desk will try again.",
            )
        }
    }
}

/// `GET /api/desk-knows/status`.
pub async fn status(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    let live = match authenticate(&state, &headers, false) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match store.status(&live.owner).await {
        Ok(s) => Json(json!({ "agentId": live.owner.agent, "held": s })).into_response(),
        Err(e) => {
            warn!(error = %format!("{e:#}"), "reading what Desk shared failed");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "This hub couldn't read that; try again.",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(v: Value) -> Result<Item, String> {
        checked(serde_json::from_value(v).unwrap())
    }

    #[test]
    fn a_pushed_item_is_checked_before_it_is_kept() {
        let at = "2026-11-13T10:00:00Z";
        assert!(item(json!({"kind": "account", "ref": "a1", "session": "s1", "group": "Patchwork", "at": at, "body": {"text": "x"}})).is_ok());
        for bad in [
            json!({"kind": "secret", "ref": "a1", "at": at, "body": {"text": "x"}}),
            json!({"kind": "account", "ref": "", "at": at, "body": {"text": "x"}}),
            json!({"kind": "account", "ref": "a\n1", "at": at, "body": {"text": "x"}}),
            json!({"kind": "account", "ref": "a1", "at": at, "body": "x"}),
            json!({"kind": "account", "ref": "a1", "at": at, "body": {"words": "x"}}),
            json!({"kind": "transcript", "ref": "a1", "at": at, "body": {"text": "x".repeat(MAX_TEXT + 1)}}),
            json!({"kind": "account", "ref": "a1", "group": "x".repeat(MAX_NAME + 1), "at": at, "body": {"text": "x"}}),
        ] {
            assert!(item(bad.clone()).is_err(), "{bad}");
        }
    }
}

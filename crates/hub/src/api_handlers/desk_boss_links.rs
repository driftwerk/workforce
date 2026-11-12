//! `/api/me/desk-boss-links`: a paired Desk mints, lists and revokes the
//! boss links it holds here, with its own device token and nothing else
//! (`super::remote_mcp_tokens::paired_device`). A login, the hub's own token
//! or a link token is refused.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use workforce_identity::remote_mcp_token::RemoteMcpToken;

use super::my_settings::enforce_action_rate_limit;
use super::remote_mcp_tokens::paired_device;
use crate::boss_links::links::{self, LinkError, MintRequest};
use crate::gateway::GatewayState;

pub(crate) type Rejection = (StatusCode, Json<serde_json::Value>);

pub(crate) fn err(status: StatusCode, msg: &str) -> Rejection {
    (status, Json(serde_json::json!({"error": msg})))
}

pub(crate) fn rejection(e: LinkError) -> Rejection {
    let status = match &e {
        LinkError::Disabled => StatusCode::NOT_FOUND,
        LinkError::IdentityNotActive | LinkError::NotAllowed | LinkError::AgentDeclines => {
            StatusCode::FORBIDDEN
        }
        LinkError::AgentUnreachable | LinkError::RenewsUnknown => StatusCode::NOT_FOUND,
        LinkError::Invalid(_) | LinkError::BadKey(_) => StatusCode::BAD_REQUEST,
        LinkError::LinkCap(_) => StatusCode::CONFLICT,
        LinkError::Store(inner) => {
            tracing::warn!(error = %inner, "Desk boss link store error");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "link store error");
        }
    };
    err(status, &e.to_string())
}

#[derive(Deserialize)]
pub struct GroupBody {
    key: String,
    name: String,
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
    group: GroupBody,
    engine: EngineBody,
    #[serde(default)]
    renews: Option<Uuid>,
}

#[derive(Serialize)]
pub struct MintedView {
    id: Uuid,
    link_id: Uuid,
    token: String,
    expires_at: DateTime<Utc>,
    /// The name this hub goes by in an uplink's challenge: the engine dials
    /// it, and signs it.
    hub: Option<String>,
}

#[derive(Serialize)]
pub struct LinkView {
    id: Uuid,
    link_id: Uuid,
    label: String,
    /// Non-secret label derived from the stored hash.
    fingerprint: String,
    agent_id: String,
    group_key: String,
    group_name: String,
    engine_id: String,
    engine_name: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

impl LinkView {
    fn new(token: RemoteMcpToken) -> Option<Self> {
        let link = token.desk_link.clone()?;
        Some(Self {
            fingerprint: token.fingerprint(),
            id: token.id,
            link_id: link.link_id,
            label: token.label,
            agent_id: link.agent_id,
            // Desk's own key for its group; the hub's is derived from it.
            group_key: link.desk_group_key,
            group_name: link.group_name,
            engine_id: link.engine_id,
            engine_name: link.engine_name,
            created_at: token.created_at,
            expires_at: token.expires_at,
            last_used_at: token.last_used_at,
        })
    }
}

#[derive(Serialize)]
pub struct LinkListView {
    links: Vec<LinkView>,
}

/// `POST /api/me/desk-boss-links`: a paired Desk links an agent here as
/// the boss of one of its person's groups, for one engine. Answers the
/// token once; it is never shown again.
pub async fn create(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<MintBody>,
) -> Result<(StatusCode, Json<MintedView>), Rejection> {
    links::require_enabled(&state).map_err(rejection)?;
    let (device, person) = paired_device(&state, &headers).await?;
    enforce_action_rate_limit(&state, &person.to_string())?;
    let req = MintRequest {
        agent_id: body.agent_id,
        group_key: body.group.key,
        group_name: body.group.name,
        engine_id: body.engine.id,
        engine_name: body.engine.name,
        engine_public_key: body.engine.public_key,
        renews: body.renews,
    };
    let minted = links::mint(&state, person, &device.id, &req).map_err(rejection)?;
    Ok((
        StatusCode::CREATED,
        Json(MintedView {
            id: minted.record.id,
            link_id: minted.link.link_id,
            token: minted.plaintext,
            expires_at: minted.record.expires_at,
            hub: crate::boss_links::auth::this_hub(&state),
        }),
    ))
}

/// `GET /api/me/desk-boss-links`: the live links this Desk holds here, and
/// no one else's. No token, nor any part of one.
pub async fn list(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Result<Json<LinkListView>, Rejection> {
    links::require_enabled(&state).map_err(rejection)?;
    let (device, person) = paired_device(&state, &headers).await?;
    let links = links::list(&state, person, &device.id)
        .map_err(rejection)?
        .into_iter()
        .filter_map(LinkView::new)
        .collect();
    Ok(Json(LinkListView { links }))
}

/// `DELETE /api/me/desk-boss-links/{id}`: this Desk ends one of its links.
/// Only its own: any other id is "not found", whoever holds it. Taken even
/// while `[desk_boss]` is off, so a link can always be stopped.
pub async fn revoke(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, Rejection> {
    let (device, person) = paired_device(&state, &headers).await?;
    enforce_action_rate_limit(&state, &person.to_string())?;
    let id = Uuid::parse_str(&id).map_err(|_| err(StatusCode::NOT_FOUND, "Link not found"))?;
    links::revoke(&state, person, &device.id, id)
        .map_err(rejection)?
        .then_some(StatusCode::NO_CONTENT)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Link not found"))
}

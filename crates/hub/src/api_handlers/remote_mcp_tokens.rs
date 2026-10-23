//! Remote MCP token management — `/api/me/remote-mcp-tokens` for the caller's
//! own tokens (login JWT only) and `/api/remote-mcp-tokens` for admins.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use workforce_identity::remote_mcp_token::RemoteMcpToken;

use super::identity::{require_auth, require_roles_assign};
use super::my_settings::enforce_action_rate_limit;
use crate::gateway::GatewayState;
use crate::remote_mcp::tokens::{is_expired, RemoteMcpTokenService, TokenError};

type Rejection = (StatusCode, Json<serde_json::Value>);
type ApiResult<T> = Result<Json<T>, Rejection>;

fn err(status: StatusCode, msg: &str) -> Rejection {
    (status, Json(serde_json::json!({"error": msg})))
}

fn rejection(e: TokenError) -> Rejection {
    match e {
        TokenError::Disabled => err(StatusCode::NOT_FOUND, &e.to_string()),
        TokenError::InvalidLabel => err(StatusCode::BAD_REQUEST, &e.to_string()),
        TokenError::IdentityNotActive => err(StatusCode::FORBIDDEN, &e.to_string()),
        TokenError::TokenCap(_) => err(StatusCode::CONFLICT, &e.to_string()),
        TokenError::Store(inner) => {
            tracing::warn!(error = %inner, "remote MCP token store error");
            err(StatusCode::INTERNAL_SERVER_ERROR, "token store error")
        }
    }
}

/// The identity a login JWT carries. A device-bound token never manages
/// remote MCP tokens: a device acts as a Member, while a minted token acts
/// with the identity's full role.
fn human_identity(claims: &crate::jwt::Claims) -> Result<Uuid, Rejection> {
    if claims.device_id.is_some() {
        return Err(err(
            StatusCode::FORBIDDEN,
            "This endpoint requires a user login, not a device token",
        ));
    }
    claims
        .identity_id
        .as_deref()
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| {
            err(
                StatusCode::FORBIDDEN,
                "This endpoint requires a user identity (not a static admin token)",
            )
        })
}

fn require_identity(state: &GatewayState, headers: &HeaderMap) -> Result<Uuid, Rejection> {
    human_identity(&require_auth(state, headers)?)
}

fn service(state: &GatewayState) -> Result<RemoteMcpTokenService, Rejection> {
    let service = RemoteMcpTokenService::for_gateway(state).ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Identity store not configured",
        )
    })?;
    service
        .enabled()
        .then_some(service)
        .ok_or_else(|| rejection(TokenError::Disabled))
}

fn parse_id(id: &str) -> Result<Uuid, Rejection> {
    Uuid::parse_str(id).map_err(|_| err(StatusCode::NOT_FOUND, "Token not found"))
}

fn revoked(revoked: bool) -> Result<StatusCode, Rejection> {
    revoked
        .then_some(StatusCode::NO_CONTENT)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Token not found"))
}

#[derive(Serialize)]
pub struct TokenView {
    id: Uuid,
    label: String,
    token_prefix: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
    expired: bool,
}

impl TokenView {
    fn new(token: RemoteMcpToken, now: DateTime<Utc>) -> Self {
        Self {
            expired: is_expired(&token, now),
            id: token.id,
            label: token.label,
            token_prefix: token.token_prefix,
            created_at: token.created_at,
            expires_at: token.expires_at,
            last_used_at: token.last_used_at,
        }
    }
}

#[derive(Serialize)]
pub struct TokenListResponse<T> {
    tokens: Vec<T>,
}

#[derive(Serialize)]
pub struct AdminTokenView {
    #[serde(flatten)]
    token: TokenView,
    identity_id: Uuid,
    display_name: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateTokenBody {
    label: String,
}

#[derive(Serialize)]
pub struct ConnectView {
    claude_code: String,
    workforce_local: String,
}

#[derive(Serialize)]
pub struct CreatedTokenResponse {
    id: Uuid,
    label: String,
    token: String,
    expires_at: DateTime<Utc>,
    connect: ConnectView,
}

pub async fn list_my_tokens(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> ApiResult<TokenListResponse<TokenView>> {
    let identity_id = require_identity(&state, &headers)?;
    let now = Utc::now();
    let tokens = service(&state)?
        .list(identity_id)
        .map_err(rejection)?
        .into_iter()
        .map(|t| TokenView::new(t, now))
        .collect();
    Ok(Json(TokenListResponse { tokens }))
}

pub async fn create_my_token(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<CreateTokenBody>,
) -> Result<(StatusCode, Json<CreatedTokenResponse>), Rejection> {
    let identity = require_identity(&state, &headers)?;
    enforce_action_rate_limit(&state, &identity.to_string())?;
    let minted = service(&state)?
        .mint(identity, &body.label)
        .map_err(rejection)?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedTokenResponse {
            id: minted.record.id,
            label: minted.record.label,
            token: minted.plaintext,
            expires_at: minted.record.expires_at,
            connect: ConnectView {
                claude_code: minted.connect.claude_code,
                workforce_local: minted.connect.workforce_local,
            },
        }),
    ))
}

pub async fn revoke_my_token(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, Rejection> {
    let identity = require_identity(&state, &headers)?;
    enforce_action_rate_limit(&state, &identity.to_string())?;
    let done = service(&state)?
        .revoke(identity, parse_id(&id)?)
        .map_err(rejection)?;
    revoked(done)
}

pub async fn list_all_tokens(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> ApiResult<TokenListResponse<AdminTokenView>> {
    require_roles_assign(&state, &headers).await?;
    let service = service(&state)?;
    let now = Utc::now();
    let tokens = service
        .list_all()
        .map_err(rejection)?
        .into_iter()
        .map(|t| AdminTokenView {
            identity_id: t.identity_id,
            display_name: service.display_name(t.identity_id),
            token: TokenView::new(t, now),
        })
        .collect();
    Ok(Json(TokenListResponse { tokens }))
}

pub async fn revoke_any_token(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, Rejection> {
    require_roles_assign(&state, &headers).await?;
    let done = service(&state)?
        .revoke_any(parse_id(&id)?)
        .map_err(rejection)?;
    revoked(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_identity::types::IdentityRole;

    fn claims(identity_id: Option<&str>, device_id: Option<&str>) -> crate::jwt::Claims {
        crate::jwt::Claims {
            sub: "someone".into(),
            role: IdentityRole::SystemAdmin,
            identity_id: identity_id.map(str::to_owned),
            iat: 0,
            exp: u64::MAX,
            scopes: vec!["*".into()],
            device_id: device_id.map(str::to_owned),
        }
    }

    #[test]
    fn only_a_human_login_manages_tokens() {
        let id = Uuid::new_v4();
        assert_eq!(
            human_identity(&claims(Some(&id.to_string()), None)).unwrap(),
            id
        );
        for rejected in [
            claims(Some(&id.to_string()), Some("ios-device")),
            claims(None, None),
            claims(Some("not-a-uuid"), None),
        ] {
            assert_eq!(
                human_identity(&rejected).unwrap_err().0,
                StatusCode::FORBIDDEN
            );
        }
    }
}

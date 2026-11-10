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
        TokenError::DevicesNotAllowed => err(StatusCode::FORBIDDEN, &e.to_string()),
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
    /// Non-secret label derived from the stored hash; names the key, reveals none of it.
    fingerprint: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
    expired: bool,
    /// The paired device that minted it, when one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    minted_by_device: Option<String>,
}

impl TokenView {
    fn new(token: RemoteMcpToken, now: DateTime<Utc>) -> Self {
        Self {
            expired: is_expired(&token, now),
            minted_by_device: crate::remote_mcp::tokens::minted_by_device(&token.scopes)
                .map(str::to_owned),
            fingerprint: token.fingerprint(),
            id: token.id,
            label: token.label,
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

/// The paired device a request's bearer is, and the person it is linked
/// to. Only a device token is taken here: a login or the hub's own token is
/// refused, so this route is never a second way for a person to mint.
async fn paired_device(
    state: &GatewayState,
    headers: &HeaderMap,
) -> Result<(workforce_pairing::ApprovedDevice, Uuid), Rejection> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthorized"))?;
    let pairing = state.device_pairing.as_ref().ok_or_else(|| {
        err(
            StatusCode::FORBIDDEN,
            "This endpoint takes a paired device's token",
        )
    })?;
    let device = match pairing.validate_token_async(bearer.to_string()).await {
        Ok(Some(device)) => device,
        Ok(None) => {
            return Err(err(
                StatusCode::FORBIDDEN,
                "This endpoint takes a paired device's token",
            ))
        }
        Err(e) => {
            tracing::warn!(error = %e, "pairing store error minting a device's remote MCP token");
            return Err(err(
                StatusCode::SERVICE_UNAVAILABLE,
                "the hub could not check that device",
            ));
        }
    };
    let person = state
        .identity_store
        .as_ref()
        .and_then(|store| crate::device_link::identity_for_device(store.as_ref(), &device.id))
        .and_then(|id| Uuid::parse_str(&id).ok())
        .ok_or_else(|| {
            err(
                StatusCode::FORBIDDEN,
                "This device isn't linked to a person here, so it can't act for anyone",
            )
        })?;
    // Only Desk on a computer mints: not a phone, a voice box or the rest.
    let desktop = state
        .identity_store
        .as_ref()
        .is_some_and(|store| crate::device_link::is_desktop_device(store.as_ref(), &device.id));
    if !desktop {
        return Err(err(
            StatusCode::FORBIDDEN,
            "Only Desk on a computer makes its own token here",
        ));
    }
    Ok((device, person))
}

/// `POST /api/me/remote-mcp-tokens/desk`: a paired Desk mints a token for
/// the person it is linked to, when `[remote_mcp] allow_device_tokens` is
/// on. The token is the person's with a member's capabilities only, named
/// "Desk on <device>", and dies with the device's pairing.
pub async fn create_device_token(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<CreatedTokenResponse>), Rejection> {
    let service = service(&state)?;
    let (device, person) = paired_device(&state, &headers).await?;
    enforce_action_rate_limit(&state, &person.to_string())?;
    let minted = service
        .mint_for_device(person, &device.id, &device.device_name)
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
    fn a_listed_key_carries_a_fingerprint_and_no_part_of_the_key() {
        let minted = workforce_identity::remote_mcp_token::mint();
        let body = minted.plaintext["wfr_".len()..].to_string();
        let now = Utc::now();
        let token = RemoteMcpToken {
            id: Uuid::new_v4(),
            identity_id: Uuid::new_v4(),
            label: "laptop".into(),
            token_hash: minted.hash,
            scopes: vec!["remote_mcp".into()],
            created_at: now,
            expires_at: now + chrono::Duration::days(90),
            last_used_at: None,
            revoked_at: None,
        };
        let expected = token.fingerprint();
        let json = serde_json::to_value(TokenView::new(token, now)).unwrap();
        assert_eq!(json["fingerprint"], expected.as_str());
        assert!(json.get("token_prefix").is_none());
        let text = json.to_string();
        for start in 0..=body.len() - 6 {
            assert!(!text.contains(&body[start..start + 6]));
        }
    }

    async fn mint_as(
        state: &Arc<GatewayState>,
        bearer: &str,
    ) -> Result<serde_json::Value, StatusCode> {
        create_device_token(
            State(Arc::clone(state)),
            crate::remote_mcp::testing::bearer(bearer),
        )
        .await
        .map(|(_, Json(body))| serde_json::to_value(body).unwrap())
        .map_err(|(status, _)| status)
    }

    /// The four answers the device-token decision asked for, at the route:
    /// off is refused, on mints the identity's member-scoped token, and
    /// neither a login nor the hub's own token mints here.
    #[tokio::test]
    async fn a_paired_device_mints_only_where_the_hub_allows_it() {
        use crate::remote_mcp::testing::{fixture_with_devices, pair_device};
        use workforce_identity::store::IdentityStore;
        for allowed in [false, true] {
            let (fx, devices) = fixture_with_devices(allowed);
            let mut me = workforce_identity::types::Identity::new("Sam", IdentityRole::Member);
            fx.identities.create(&me).unwrap();
            let (device, bearer) = pair_device(&fx, &devices, &mut me, "Sam's Mac");
            let minted = mint_as(&fx.state, &bearer).await;
            if !allowed {
                assert_eq!(minted.unwrap_err(), StatusCode::FORBIDDEN);
                continue;
            }
            let body = minted.unwrap();
            assert_eq!(body["label"], "Desk on Sam's Mac");
            let token = body["token"].as_str().unwrap();
            let caller = crate::remote_mcp::auth::authenticate(
                &fx.state,
                &crate::remote_mcp::testing::bearer(token),
            )
            .unwrap();
            assert_eq!(caller.identity.id, me.id);
            assert!(caller.as_member);
            assert_eq!(
                crate::remote_mcp::tokens::minted_by_device(&caller.token.scopes),
                Some(device.id.as_str())
            );
            // A phone paired to the same person doesn't mint.
            let (_, phone) = crate::remote_mcp::testing::pair_device_as(
                &fx,
                &devices,
                &mut me,
                "Sam's phone",
                "ios_device",
            );
            assert_eq!(
                mint_as(&fx.state, &phone).await.unwrap_err(),
                StatusCode::FORBIDDEN
            );
            for not_a_device in ["gateway-token", "eyJhbGciOi.not.a.jwt"] {
                assert_eq!(
                    mint_as(&fx.state, not_a_device).await.unwrap_err(),
                    StatusCode::FORBIDDEN
                );
            }
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

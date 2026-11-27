//! The one door every bearer credential comes through. [`admit`] recognises
//! the credential (the static gateway token, a JWT, a paired device's token),
//! decides what the surface it arrived on lets it be ([`ceiling`]), and reads
//! the person's live role under that ceiling. Nothing outside this module
//! validates a JWT.

pub mod jwt;

use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use subtle::ConstantTimeEq;
use tracing::warn;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::IdentityRole;
use workforce_pairing::{ApprovedDevice, DevicePairingService, TokenMatch};

use crate::authz::Ceiling;
use crate::gateway::{AuthFailure, CallerContext, GatewayState, ScopeRejection};
use crate::surface::{Arrival, Surface};
use jwt::{Claims, JwtConfig};

/// Constant-time token comparison to prevent timing side-channel attacks.
/// Hashes both tokens with blake3 first to ensure fixed-length comparison.
pub fn constant_time_token_eq(a: &str, b: &str) -> bool {
    // An empty token must never authenticate. Without this guard, an unset /
    // empty configured token (`state.auth_token = ""`) matches a tokenless
    // caller (`eq("", "")` was `true`), collapsing the admin checks in
    // `check_owner_auth`, `ws_local_agent_handler`, etc. to "anyone is admin"
    // (audit F2 — and the dependency under F1's `check_owner_auth` path).
    // Whether a string is empty is not secret, so this early return leaks no
    // timing about the secret's content.
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let hash_a = blake3::hash(a.as_bytes());
    let hash_b = blake3::hash(b.as_bytes());
    hash_a.as_bytes().ct_eq(hash_b.as_bytes()).into()
}

/// What a hub admits bearer credentials against.
#[derive(Clone, Copy)]
pub(crate) struct Door<'a> {
    pub jwt: Option<&'a JwtConfig>,
    pub static_token: Option<&'a str>,
    pub pairing: Option<&'a DevicePairingService>,
    pub identities: Option<&'a Arc<dyn IdentityStore>>,
}

impl GatewayState {
    pub(crate) fn door(&self) -> Door<'_> {
        Door {
            jwt: self.jwt_config.as_ref(),
            static_token: Some(&self.auth_token),
            pairing: self.device_pairing.as_deref(),
            identities: self.identity_store.as_ref(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialKind {
    StaticToken,
    /// `unscoped`: the token carried no scopes, as tokens minted before
    /// scopes existed do.
    Jwt {
        unscoped: bool,
    },
    Device(TokenMatch),
}

/// An admitted credential: its claims, already carrying the person's live
/// role held to `ceiling`.
#[derive(Debug, Clone)]
pub(crate) struct Admitted {
    pub claims: Claims,
    pub ceiling: Ceiling,
    pub kind: CredentialKind,
}

/// An admitted paired device, its scopes already held to `ceiling`.
#[derive(Debug, Clone)]
pub(crate) struct Paired {
    pub device: ApprovedDevice,
    pub token_match: TokenMatch,
    pub ceiling: Ceiling,
}

/// What a credential of `kind` may be on the surface it arrived by. The
/// combined surface caps nothing, as a hub with one listener always has; the
/// public surface holds everyone to a member and refuses the static token;
/// the admin surface takes only its own JWTs, and the static token from the
/// host itself.
pub(crate) fn ceiling(arrival: Arrival, kind: CredentialKind) -> Result<Ceiling, AuthFailure> {
    match (arrival.surface, kind) {
        (Surface::Combined, _) => Ok(Ceiling::Full),
        (Surface::Public, CredentialKind::StaticToken) => Err(AuthFailure::SurfaceRefused),
        (Surface::Public, _) => Ok(Ceiling::Member),
        (Surface::Admin, CredentialKind::Jwt { .. }) => Ok(Ceiling::Full),
        (Surface::Admin, CredentialKind::StaticToken)
            if arrival
                .peer
                .is_some_and(|peer| peer.to_canonical().is_loopback()) =>
        {
            Ok(Ceiling::Full)
        }
        (Surface::Admin, _) => Err(AuthFailure::SurfaceRefused),
    }
}

/// Admits `token` as whichever credential it is, on the surface in scope.
pub(crate) async fn admit(door: Door<'_>, token: &str) -> Result<Admitted, AuthFailure> {
    match admit_signed(door, token) {
        Some(admitted) => admitted,
        None => admit_device(door, token).await,
    }
}

/// [`admit`] for the credentials the hub checks without a store: the static
/// token and JWTs. `None` when `token` is neither.
pub(crate) fn admit_signed(door: Door<'_>, token: &str) -> Option<Result<Admitted, AuthFailure>> {
    let arrival = crate::surface::current();
    let (kind, claims) = match door
        .static_token
        .is_some_and(|ours| constant_time_token_eq(token, ours))
    {
        true => (CredentialKind::StaticToken, hub_owner()),
        false => {
            let claims =
                jwt::validate_token(&door.jwt?.surface_key(arrival.surface), token).ok()?;
            let unscoped = claims.scopes.is_empty();
            (CredentialKind::Jwt { unscoped }, claims)
        }
    };
    Some(ceiling(arrival, kind).map(|ceiling| Admitted {
        claims: jwt::current_claims(door.identities, claims, ceiling),
        ceiling,
        kind,
    }))
}

/// [`admit`] for the routes only a paired device may call: the static token
/// and JWTs are not taken here.
pub(crate) async fn admit_paired(door: Door<'_>, token: &str) -> Result<Paired, AuthFailure> {
    let pairing = door.pairing.ok_or(AuthFailure::InvalidCredentials)?;
    let (device, token_match) = pairing
        .validate_token_with_match_async(token.to_string())
        .await
        .map_err(|e| {
            warn!(
                token = %crate::token_hint::hint(token),
                error = %e,
                "Pairing store error while validating a device token — failing closed"
            );
            AuthFailure::StoreError
        })?
        .ok_or(AuthFailure::InvalidCredentials)?;
    let ceiling = ceiling(
        crate::surface::current(),
        CredentialKind::Device(token_match),
    )?;
    Ok(Paired {
        device: ApprovedDevice {
            scopes: jwt::capped_scopes(device.scopes, ceiling),
            ..device
        },
        token_match,
        ceiling,
    })
}

/// Whether `token` is the hub's own static token, on a surface that takes it.
pub(crate) fn is_hub_token(door: Door<'_>, token: &str) -> bool {
    admit_signed(door, token)
        .and_then(Result::ok)
        .is_some_and(|admitted| admitted.kind == CredentialKind::StaticToken)
}

/// Whether `headers` carry the hub's own static token as their bearer, on a
/// surface that takes it.
pub(crate) fn bears_hub_token(door: Door<'_>, headers: &axum::http::HeaderMap) -> bool {
    crate::gateway::header_bearer(headers).is_some_and(|token| is_hub_token(door, token))
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

/// What stands before every admin route: a credential, from the `token`
/// query or the `Authorization` header, admitted as someone beyond a member
/// (an active administrator, manager or custom role, under no ceiling).
/// Anything else is refused before the route's extractors run: 401 when no
/// credential is admitted, 403 when only a member's is.
pub async fn require_beyond_member(
    State(state): State<Arc<GatewayState>>,
    request: Request,
    next: Next,
) -> Response {
    let tokens: Vec<String> = Query::<TokenQuery>::try_from_uri(request.uri())
        .ok()
        .and_then(|Query(query)| query.token)
        .into_iter()
        .chain(crate::gateway::header_bearer(request.headers()).map(str::to_string))
        .filter(|token| !token.is_empty())
        .collect();
    let mut refusal = ScopeRejection::Unauthenticated;
    for token in &tokens {
        if let Ok(admitted) = admit(state.door(), token).await {
            let caller = CallerContext::from(admitted);
            if crate::gateway::gateway_principal(&caller, state.identity_store.as_ref())
                .is_beyond_member()
            {
                return next.run(request).await;
            }
            refusal = ScopeRejection::MissingScope;
        }
    }
    refusal.into_response()
}

async fn admit_device(door: Door<'_>, token: &str) -> Result<Admitted, AuthFailure> {
    let Paired {
        device,
        token_match,
        ceiling,
    } = admit_paired(door, token).await?;
    let identity_id = door
        .identities
        .and_then(|store| crate::device_link::identity_for_device(store.as_ref(), &device.id));
    Ok(Admitted {
        claims: Claims {
            sub: device.device_name,
            role: IdentityRole::Member,
            identity_id,
            iat: 0,
            exp: u64::MAX,
            scopes: device.scopes,
            device_id: Some(device.id),
        },
        ceiling,
        kind: CredentialKind::Device(token_match),
    })
}

/// The static gateway token's claims: the hub's owner, who is no one.
fn hub_owner() -> Claims {
    Claims {
        sub: "admin".to_string(),
        role: IdentityRole::SystemAdmin,
        identity_id: None,
        iat: 0,
        exp: u64::MAX,
        scopes: vec![jwt::SCOPE_WILDCARD.to_string()],
        device_id: None,
    }
}

#[cfg(test)]
mod tests;

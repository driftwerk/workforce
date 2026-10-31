//! Bearer authentication for the remote MCP routes — and only for them.
//!
//! `wfr_` tokens are deliberately unknown to `gateway::resolve_caller_context`:
//! several REST handlers gate on "is authenticated" alone, so a token that
//! resolved there would reach routes it must never reach.

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{Duration, Utc};
use tracing::warn;

use workforce_identity::remote_mcp_token::{self, RemoteMcpToken};
use workforce_identity::types::{Identity, IdentityStatus};

use crate::authz::Principal;
use crate::gateway::GatewayState;

use super::tokens::TOKEN_SCOPE;

/// `last_used_at` is refreshed at most this often, so a busy client does not
/// turn every call into a store write.
const LAST_USED_REFRESH: Duration = Duration::minutes(5);

/// Every scope this hub can enforce. `remote_mcp` is the whole remote
/// surface and the only scope minted so far.
const KNOWN_SCOPES: &[&str] = &[TOKEN_SCOPE];

/// The user a remote MCP request acts as, resolved live on every request so a
/// suspension or role change takes effect on the next call.
pub struct RemoteCaller {
    pub token: RemoteMcpToken,
    pub identity: Identity,
    pub principal: Principal,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    Unauthenticated,
    NotActive,
    /// The token's scopes don't grant the remote surface, or name one this
    /// hub can't enforce.
    OutOfScope,
    Unavailable,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthenticated => (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "a live remote MCP token is required",
            )
                .into_response(),
            Self::NotActive => (StatusCode::FORBIDDEN, "identity is not active").into_response(),
            Self::OutOfScope => (
                StatusCode::FORBIDDEN,
                "this token's scopes do not cover remote MCP on this hub",
            )
                .into_response(),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "remote MCP token store unavailable",
            )
                .into_response(),
        }
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .filter(|t| t.starts_with(remote_mcp_token::TOKEN_PREFIX))
}

pub fn authenticate(state: &GatewayState, headers: &HeaderMap) -> Result<RemoteCaller, AuthError> {
    let presented = bearer(headers).ok_or(AuthError::Unauthenticated)?;
    let (Some(tokens), Some(identities)) = (&state.remote_mcp_tokens, &state.identity_store) else {
        return Err(AuthError::Unavailable);
    };
    let now = Utc::now();
    let token = tokens
        .get_token_by_hash(&remote_mcp_token::hash_token(presented))
        .map_err(|e| {
            warn!(error = %e, "remote MCP token lookup failed — failing closed");
            AuthError::Unavailable
        })?
        .filter(|t| t.is_live(now))
        .ok_or(AuthError::Unauthenticated)?;
    if !covers_remote_surface(&token.scopes) {
        return Err(AuthError::OutOfScope);
    }
    let identity = identities
        .get_by_id(token.identity_id)
        .map_err(|e| {
            warn!(error = %e, "identity lookup for remote MCP token failed — failing closed");
            AuthError::Unavailable
        })?
        .ok_or(AuthError::Unauthenticated)?;
    if identity.status != IdentityStatus::Active {
        return Err(AuthError::NotActive);
    }
    let stale = token
        .last_used_at
        .is_none_or(|used| now - used > LAST_USED_REFRESH);
    if stale {
        if let Err(e) = tokens.touch_token_last_used(token.id, now) {
            warn!(error = %e, token = %token.id, "could not record remote MCP token use");
        }
    }
    let principal = Principal {
        identity_id: Some(identity.id),
        role: identity.role,
        role_key: identity.effective_role_key().to_string(),
        status: identity.status,
    };
    Ok(RemoteCaller {
        token,
        identity,
        principal,
    })
}

/// Whether a token with `scopes` may use the whole remote surface: it must
/// carry `remote_mcp`, and nothing this hub doesn't know. A scope it doesn't
/// know may be one meant to narrow the token, so the token is refused
/// rather than given everything.
fn covers_remote_surface(scopes: &[String]) -> bool {
    scopes.iter().any(|s| s == TOKEN_SCOPE)
        && scopes.iter().all(|s| KNOWN_SCOPES.contains(&s.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_mcp::testing::{bearer, fixture, fixture_with, issue, TokenShape};
    use workforce_identity::store::IdentityStore;
    use workforce_identity::types::IdentityRole;

    fn active(fx: &crate::remote_mcp::testing::Fixture, role: IdentityRole) -> Identity {
        let identity = Identity::new("Someone", role);
        fx.identities.create(&identity).unwrap();
        identity
    }

    #[test]
    fn a_missing_or_foreign_bearer_is_unauthenticated() {
        let fx = fixture(true);
        assert_eq!(
            authenticate(&fx.state, &HeaderMap::new()).err(),
            Some(AuthError::Unauthenticated)
        );
        for foreign in ["not-a-token", "wfp_device-token", "gateway-token"] {
            assert_eq!(
                authenticate(&fx.state, &bearer(foreign)).err(),
                Some(AuthError::Unauthenticated),
                "{foreign}"
            );
        }
    }

    #[test]
    fn an_unknown_expired_or_revoked_token_is_unauthenticated() {
        let fx = fixture(true);
        let who = active(&fx, IdentityRole::Member);
        let unknown = remote_mcp_token::mint().plaintext;
        let expired = issue(
            &fx,
            &who,
            TokenShape {
                expires_at: Utc::now() - Duration::seconds(1),
                ..Default::default()
            },
        );
        let revoked = issue(
            &fx,
            &who,
            TokenShape {
                revoked_at: Some(Utc::now()),
                ..Default::default()
            },
        );
        for token in [unknown, expired, revoked] {
            assert_eq!(
                authenticate(&fx.state, &bearer(&token)).err(),
                Some(AuthError::Unauthenticated)
            );
        }
    }

    #[test]
    fn a_suspended_identity_is_forbidden_even_with_a_live_token() {
        let fx = fixture(true);
        let mut who = active(&fx, IdentityRole::SystemAdmin);
        let token = issue(&fx, &who, TokenShape::default());
        who.status = IdentityStatus::Suspended;
        fx.identities.update(&who).unwrap();
        assert_eq!(
            authenticate(&fx.state, &bearer(&token)).err(),
            Some(AuthError::NotActive)
        );
    }

    #[test]
    fn a_live_token_resolves_the_identitys_real_role() {
        let fx = fixture(true);
        let admin = active(&fx, IdentityRole::SystemAdmin);
        let member = active(&fx, IdentityRole::Member);
        let admin_token = issue(&fx, &admin, TokenShape::default());
        let member_token = issue(&fx, &member, TokenShape::default());

        let caller = authenticate(&fx.state, &bearer(&admin_token)).unwrap();
        assert_eq!(caller.identity.id, admin.id);
        assert_eq!(caller.principal.role, IdentityRole::SystemAdmin);
        assert_eq!(caller.principal.role_key, "system_admin");
        assert_eq!(caller.principal.identity_id, Some(admin.id));
        assert_eq!(caller.token.identity_id, admin.id);

        let caller = authenticate(&fx.state, &bearer(&member_token)).unwrap();
        assert_eq!(caller.principal.role, IdentityRole::Member);
    }

    #[test]
    fn last_used_is_recorded_only_when_stale() {
        let fx = fixture(true);
        let who = active(&fx, IdentityRole::Member);
        let never = issue(&fx, &who, TokenShape::default());
        let fresh_at = Utc::now() - Duration::minutes(1);
        let fresh = issue(
            &fx,
            &who,
            TokenShape {
                last_used_at: Some(fresh_at),
                ..Default::default()
            },
        );
        let stale_at = Utc::now() - Duration::minutes(10);
        let stale = issue(
            &fx,
            &who,
            TokenShape {
                last_used_at: Some(stale_at),
                ..Default::default()
            },
        );

        let used = |token: &str| authenticate(&fx.state, &bearer(token)).unwrap().token.id;
        let after = |id: uuid::Uuid| {
            fx.tokens
                .list_tokens_for_identity(who.id)
                .unwrap()
                .into_iter()
                .find(|t| t.id == id)
                .unwrap()
                .last_used_at
        };

        let never_id = used(&never);
        assert!(after(never_id).is_some(), "first use is recorded");
        let fresh_id = used(&fresh);
        assert_eq!(
            after(fresh_id),
            Some(fresh_at),
            "a recent use is not rewritten"
        );
        let stale_id = used(&stale);
        assert!(
            after(stale_id).unwrap() > stale_at,
            "a stale use is refreshed"
        );
    }

    #[test]
    fn a_token_is_held_to_its_scopes() {
        let fx = fixture(true);
        let who = active(&fx, IdentityRole::Member);
        let scoped = |scopes: &[&str]| {
            issue(
                &fx,
                &who,
                TokenShape {
                    scopes: scopes.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                },
            )
        };
        assert!(authenticate(&fx.state, &bearer(&scoped(&[TOKEN_SCOPE]))).is_ok());
        for scopes in [&[][..], &["agent.chat"], &[TOKEN_SCOPE, "read_only"]] {
            assert_eq!(
                authenticate(&fx.state, &bearer(&scoped(scopes))).err(),
                Some(AuthError::OutOfScope),
                "{scopes:?}"
            );
        }
    }

    #[test]
    fn a_missing_store_fails_closed_as_unavailable() {
        let fx = fixture_with(true, false);
        let who = active(&fx, IdentityRole::Member);
        let token = issue(&fx, &who, TokenShape::default());
        assert_eq!(
            authenticate(&fx.state, &bearer(&token)).err(),
            Some(AuthError::Unavailable)
        );
    }
}

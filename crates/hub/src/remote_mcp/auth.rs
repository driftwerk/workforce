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

use workforce_identity::remote_mcp_token::TokenScope;

use super::tokens::minted_by_device;

/// `last_used_at` is refreshed at most this often, so a busy client does not
/// turn every call into a store write.
const LAST_USED_REFRESH: Duration = Duration::minutes(5);

/// The user a remote MCP request acts as, resolved live on every request so a
/// suspension or role change takes effect on the next call.
pub struct RemoteCaller {
    pub token: RemoteMcpToken,
    pub identity: Identity,
    pub principal: Principal,
    /// The token was minted by a paired device: its calls are held to a
    /// member's capabilities whatever the person's own role.
    pub as_member: bool,
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
    if token.scope() != Some(TokenScope::RemoteMcp) {
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
    let as_member = match minted_by_device(&token.scopes) {
        Some(device) => {
            device_still_paired(state, device, &token)?;
            true
        }
        None => false,
    };
    let stale = token
        .last_used_at
        .is_none_or(|used| now - used > LAST_USED_REFRESH);
    if stale {
        if let Err(e) = tokens.touch_token_last_used(token.id, now) {
            warn!(error = %e, token = %token.id, "could not record remote MCP token use");
        }
    }
    let principal = match as_member {
        true => Principal {
            identity_id: Some(identity.id),
            ..crate::cap_gate::fallback_principal()
        },
        false => Principal {
            identity_id: Some(identity.id),
            role: identity.role,
            role_key: identity.effective_role_key().to_string(),
            status: identity.status,
        },
    };
    Ok(RemoteCaller {
        token,
        identity,
        principal,
        as_member,
    })
}

/// Whether the device that minted a token is still paired to the token's
/// person, as the same pairing that minted it, checked on every request:
/// unpairing, expiring or reassigning the device, by any route, ends its
/// tokens at once, and a device paired again (a new pairing under the same
/// id) never revives a token minted before it. A token found dead is
/// revoked on the spot, so it leaves the listing and the cap too. Fails
/// closed when it can't be told.
pub(crate) fn device_still_paired(
    state: &GatewayState,
    device: &str,
    token: &RemoteMcpToken,
) -> Result<(), AuthError> {
    let pairing = state
        .device_pairing
        .as_ref()
        .ok_or(AuthError::Unauthenticated)?;
    let now = Utc::now();
    let found = pairing.device(device).map_err(|e| {
        warn!(error = %e, "device lookup for a device-minted remote MCP token failed — failing closed");
        AuthError::Unavailable
    })?;
    let paired = found.as_ref().is_some_and(|d| {
        !d.revoked && d.expires_at.is_none_or(|at| at > now) && d.created_at <= token.created_at
    });
    let holder = state
        .identity_store
        .as_ref()
        .and_then(|store| crate::device_link::identity_for_device(store.as_ref(), device));
    if paired && holder.as_deref() == Some(token.identity_id.to_string().as_str()) {
        return Ok(());
    }
    if let Some(tokens) = state.remote_mcp_tokens.as_deref() {
        if let Err(e) = tokens.revoke_token(token.id, None, now) {
            warn!(error = %e, token = %token.id, "a dead device token wasn't revoked");
        }
    }
    Err(AuthError::Unauthenticated)
}

/// Whether a token with `scopes` may use the whole remote surface: it must
/// be exactly a remote MCP token ([`TokenScope::of`]), with at most one
/// `device:<id>` and nothing this hub doesn't know. A scope it doesn't know
/// may be one meant to narrow the token, so the token is refused rather
/// than given everything; a Desk boss link token is never one.
#[cfg(test)]
fn covers_remote_surface(scopes: &[String]) -> bool {
    TokenScope::of(scopes) == Some(TokenScope::RemoteMcp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_mcp::testing::{bearer, fixture, fixture_with, issue, TokenShape};
    use crate::remote_mcp::tokens::{DEVICE_SCOPE_PREFIX, TOKEN_SCOPE};
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

    fn device_token(
        fx: &crate::remote_mcp::testing::Fixture,
        who: &Identity,
        device: &str,
    ) -> String {
        issue(
            fx,
            who,
            TokenShape {
                scopes: vec![TOKEN_SCOPE.into(), format!("{DEVICE_SCOPE_PREFIX}{device}")],
                ..Default::default()
            },
        )
    }

    /// Unpairing the device that minted a token ends the token on its next
    /// call, by whichever route the device was unpaired.
    #[test]
    fn a_device_minted_token_dies_with_its_device() {
        use crate::remote_mcp::testing::{fixture_with_devices, pair_device};
        let (fx, devices) = fixture_with_devices(true);
        let mut who = active(&fx, IdentityRole::Member);
        let (device, _) = pair_device(&fx, &devices, &mut who, "Mac");
        let token = device_token(&fx, &who, &device.id);
        assert!(authenticate(&fx.state, &bearer(&token)).unwrap().as_member);
        workforce_pairing::PairingStore::revoke_device(devices.as_ref(), &device.id).unwrap();
        assert_eq!(
            authenticate(&fx.state, &bearer(&token)).err(),
            Some(AuthError::Unauthenticated)
        );
        // A token naming a device the person doesn't hold is no one's.
        let (theirs, _) = {
            let mut other = active(&fx, IdentityRole::Member);
            pair_device(&fx, &devices, &mut other, "Other")
        };
        let borrowed = device_token(&fx, &who, &theirs.id);
        assert_eq!(
            authenticate(&fx.state, &bearer(&borrowed)).err(),
            Some(AuthError::Unauthenticated)
        );
    }

    /// A device paired again under the same id never revives a token it
    /// minted before; a dead token is revoked where it is found.
    #[test]
    fn a_re_paired_device_never_revives_an_older_token() {
        use crate::remote_mcp::testing::{fixture_with_devices, pair_device};
        let (fx, devices) = fixture_with_devices(true);
        let mut who = active(&fx, IdentityRole::Member);
        let (mut device, _) = pair_device(&fx, &devices, &mut who, "Mac");
        let token = device_token(&fx, &who, &device.id);
        assert!(authenticate(&fx.state, &bearer(&token)).is_ok());
        // Revoked, then paired again: the same id, a newer pairing.
        device.revoked = false;
        device.created_at = Utc::now() + Duration::seconds(5);
        workforce_pairing::PairingStore::revoke_device(devices.as_ref(), &device.id).unwrap();
        workforce_pairing::PairingStore::register_device(devices.as_ref(), &device).unwrap();
        assert_eq!(
            authenticate(&fx.state, &bearer(&token)).err(),
            Some(AuthError::Unauthenticated)
        );
        assert!(
            fx.tokens
                .list_tokens_for_identity(who.id)
                .unwrap()
                .is_empty(),
            "the dead token was revoked"
        );
    }

    /// An expired device's tokens die with it, and are revoked.
    #[test]
    fn an_expired_device_s_tokens_are_revoked() {
        use crate::remote_mcp::testing::{fixture_with_devices, pair_device};
        let (fx, devices) = fixture_with_devices(true);
        let mut who = active(&fx, IdentityRole::Member);
        let (mut device, _) = pair_device(&fx, &devices, &mut who, "Mac");
        let token = device_token(&fx, &who, &device.id);
        device.expires_at = Some(Utc::now() - Duration::seconds(1));
        workforce_pairing::PairingStore::register_device(devices.as_ref(), &device).unwrap();
        assert!(authenticate(&fx.state, &bearer(&token)).is_err());
        assert!(fx
            .tokens
            .list_tokens_for_identity(who.id)
            .unwrap()
            .is_empty());
    }

    /// An admin's device mints a member's token: its principal is a member.
    #[test]
    fn a_device_minted_token_never_carries_an_admin_role() {
        use crate::remote_mcp::testing::{fixture_with_devices, pair_device};
        let (fx, devices) = fixture_with_devices(true);
        let mut admin = active(&fx, IdentityRole::SystemAdmin);
        let (device, _) = pair_device(&fx, &devices, &mut admin, "Mac");
        let caller =
            authenticate(&fx.state, &bearer(&device_token(&fx, &admin, &device.id))).unwrap();
        assert!(caller.as_member);
        assert_eq!(caller.principal.role, IdentityRole::Member);
        assert_eq!(caller.principal.role_key, "member");
        assert_eq!(caller.principal.identity_id, Some(admin.id));
    }

    /// A Desk boss link token reaches no remote MCP route: by its prefix,
    /// and by its kind were it ever stored under the other prefix.
    #[test]
    fn a_desk_boss_token_never_reaches_remote_mcp() {
        let fx = fixture(true);
        let who = active(&fx, IdentityRole::SystemAdmin);
        let link = workforce_identity::remote_mcp_token::DeskLink {
            link_id: uuid::Uuid::new_v4(),
            agent_id: "allan".into(),
            group_key: "d/g".into(),
            group_name: "Acme".into(),
            engine_id: "devbox".into(),
            engine_name: "devbox".into(),
            engine_public_key: "ed25519:k".into(),
        };
        for kind in TokenScope::ALL {
            let minted = remote_mcp_token::mint_as(kind);
            fx.tokens
                .create_token(&RemoteMcpToken {
                    id: uuid::Uuid::new_v4(),
                    identity_id: who.id,
                    label: "boss".into(),
                    token_hash: minted.hash,
                    scopes: vec!["desk_boss".into(), "device:d1".into()],
                    created_at: Utc::now(),
                    expires_at: Utc::now() + Duration::days(30),
                    last_used_at: None,
                    revoked_at: None,
                    desk_link: Some(link.clone()),
                })
                .unwrap();
            let refused = authenticate(&fx.state, &bearer(&minted.plaintext)).err();
            let want = match kind {
                TokenScope::DeskBoss => AuthError::Unauthenticated,
                TokenScope::RemoteMcp => AuthError::OutOfScope,
            };
            assert_eq!(refused, Some(want), "{kind:?}");
        }
        assert!(!covers_remote_surface(&[
            "desk_boss".into(),
            "device:d1".into()
        ]));
        assert!(!covers_remote_surface(&[
            TOKEN_SCOPE.into(),
            "desk_boss".into(),
            "device:d1".into()
        ]));
    }

    #[test]
    fn a_malformed_device_scope_is_out_of_scope() {
        assert!(covers_remote_surface(&[
            TOKEN_SCOPE.into(),
            "device:abc".into()
        ]));
        for scopes in [
            vec![TOKEN_SCOPE.to_string(), "device:".into()],
            vec![TOKEN_SCOPE.into(), "device:a".into(), "device:b".into()],
            vec!["device:abc".to_string()],
        ] {
            assert!(!covers_remote_surface(&scopes), "{scopes:?}");
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

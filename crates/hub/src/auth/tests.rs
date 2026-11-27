use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};

use super::*;
use crate::gateway::CallerContext;
use crate::remote_mcp::testing::{bearer, fixture_with_devices, pair_device};

const STATIC_TOKEN: &str = "gateway-token";

#[test]
fn constant_time_eq_matching_tokens() {
    assert!(constant_time_token_eq(
        "secret-token-123",
        "secret-token-123"
    ));
}

#[test]
fn constant_time_eq_different_tokens() {
    assert!(!constant_time_token_eq(
        "secret-token-123",
        "secret-token-456"
    ));
}

#[test]
fn constant_time_eq_different_lengths() {
    assert!(!constant_time_token_eq("short", "a-much-longer-token"));
}

#[test]
fn constant_time_eq_empty_strings_never_match() {
    // audit F2: an empty/unset token must never authenticate — otherwise a
    // tokenless caller matches an unset `auth_token`.
    assert!(!constant_time_token_eq("", ""));
    assert!(!constant_time_token_eq("", "real-token"));
    assert!(!constant_time_token_eq("real-token", ""));
}

/// One person on the test hub and every credential they hold.
struct Person {
    identity: Identity,
    jwt: String,
    admin_jwt: String,
    device: String,
}

struct Hub {
    state: Arc<GatewayState>,
    identities: Arc<dyn IdentityStore>,
    people: Vec<Person>,
}

async fn hub() -> Hub {
    let (fixture, devices) = fixture_with_devices(false);
    let config = JwtConfig::new("an-auth-test-secret-of-32-bytes-or-more".into(), 3600).unwrap();
    let mut custom = Identity::new("Custom", IdentityRole::Member);
    custom.set_role_key("auditor");
    let roster = [
        Identity::new("Admin", IdentityRole::SystemAdmin),
        Identity::new("Manager", IdentityRole::Manager),
        custom,
        Identity::new("Member", IdentityRole::Member),
        Identity::new("Former", IdentityRole::SystemAdmin).with_status(IdentityStatus::Suspended),
    ];
    let mut people = Vec::new();
    for mut identity in roster {
        fixture.identities.create(&identity).unwrap();
        let (_, device) = pair_device(&fixture, &devices, &mut identity, "phone");
        let mint = || {
            jwt::generate_token_with_identity(
                &config,
                &identity.display_name,
                identity.role,
                Some(identity.id.to_string()),
            )
            .unwrap()
        };
        let admin_jwt = Arrival::new(Surface::Admin, None)
            .scope(async { mint() })
            .await;
        people.push(Person {
            jwt: mint(),
            admin_jwt,
            device,
            identity,
        });
    }
    let state = Arc::try_unwrap(fixture.state)
        .ok()
        .expect("the fixture's only handle");
    Hub {
        state: Arc::new(GatewayState {
            jwt_config: Some(config),
            ..state
        }),
        identities: fixture.identities,
        people,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presented {
    StaticToken,
    Jwt,
    Device,
}

/// The §4.3 matrix: what each kind of credential is on each surface, coming
/// from the host itself and from anywhere else.
const MATRIX: [(Surface, Presented, [Result<Ceiling, AuthFailure>; 2]); 9] = [
    (
        Surface::Combined,
        Presented::StaticToken,
        [Ok(Ceiling::Full), Ok(Ceiling::Full)],
    ),
    (
        Surface::Combined,
        Presented::Jwt,
        [Ok(Ceiling::Full), Ok(Ceiling::Full)],
    ),
    (
        Surface::Combined,
        Presented::Device,
        [Ok(Ceiling::Full), Ok(Ceiling::Full)],
    ),
    (
        Surface::Public,
        Presented::StaticToken,
        [
            Err(AuthFailure::SurfaceRefused),
            Err(AuthFailure::SurfaceRefused),
        ],
    ),
    (
        Surface::Public,
        Presented::Jwt,
        [Ok(Ceiling::Member), Ok(Ceiling::Member)],
    ),
    (
        Surface::Public,
        Presented::Device,
        [Ok(Ceiling::Member), Ok(Ceiling::Member)],
    ),
    (
        Surface::Admin,
        Presented::StaticToken,
        [Ok(Ceiling::Full), Err(AuthFailure::SurfaceRefused)],
    ),
    (
        Surface::Admin,
        Presented::Jwt,
        [Ok(Ceiling::Full), Ok(Ceiling::Full)],
    ),
    (
        Surface::Admin,
        Presented::Device,
        [
            Err(AuthFailure::SurfaceRefused),
            Err(AuthFailure::SurfaceRefused),
        ],
    ),
];

/// Peers, and whether each is the host itself.
const PEERS: [(Option<IpAddr>, bool); 5] = [
    (Some(IpAddr::V4(Ipv4Addr::LOCALHOST)), true),
    (Some(IpAddr::V6(Ipv6Addr::LOCALHOST)), true),
    (Some(IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped())), true),
    (Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))), false),
    (None, false),
];

/// What `presented` claims for `person` before any ceiling: the static
/// token is the hub's owner, a JWT the person's live role, a device a member.
fn uncapped(
    presented: Presented,
    person: &Identity,
) -> (CredentialKind, IdentityRole, Option<String>) {
    let id = Some(person.id.to_string());
    match presented {
        Presented::StaticToken => (CredentialKind::StaticToken, IdentityRole::SystemAdmin, None),
        Presented::Jwt => (
            CredentialKind::Jwt { unscoped: false },
            match person.status {
                IdentityStatus::Suspended => IdentityRole::Member,
                _ => person.role,
            },
            id,
        ),
        Presented::Device => (
            CredentialKind::Device(TokenMatch::Current),
            IdentityRole::Member,
            id,
        ),
    }
}

#[tokio::test]
async fn every_credential_on_every_surface_is_admitted_as_the_matrix_says() {
    let hub = hub().await;
    let member_scopes = jwt::scopes_for_role(IdentityRole::Member);
    for (surface, presented, outcomes) in MATRIX {
        for (peer, from_host) in PEERS {
            let outcome = outcomes[usize::from(!from_host)];
            for person in &hub.people {
                let token = match (presented, surface) {
                    (Presented::StaticToken, _) => STATIC_TOKEN,
                    (Presented::Jwt, Surface::Admin) => person.admin_jwt.as_str(),
                    (Presented::Jwt, _) => person.jwt.as_str(),
                    (Presented::Device, _) => person.device.as_str(),
                };
                let case = format!(
                    "{surface:?} · {presented:?} · {peer:?} · {}",
                    person.identity.display_name
                );
                let admitted = Arrival::new(surface, peer)
                    .scope(admit(hub.state.door(), token))
                    .await;
                let (admitted, ceiling) = match (admitted, outcome) {
                    (Ok(admitted), Ok(ceiling)) => (admitted, ceiling),
                    (got, want) => {
                        assert_eq!(got.map(|a| a.ceiling), want, "{case}");
                        continue;
                    }
                };
                let (kind, role, identity) = uncapped(presented, &person.identity);
                let role = match ceiling {
                    Ceiling::Full => role,
                    Ceiling::Member => IdentityRole::Member,
                };
                assert_eq!(admitted.ceiling, ceiling, "{case}");
                assert_eq!(admitted.kind, kind, "{case}");
                assert_eq!(admitted.claims.role, role, "{case}");
                assert_eq!(admitted.claims.identity_id, identity, "{case}");
                if ceiling == Ceiling::Member {
                    assert!(
                        admitted
                            .claims
                            .scopes
                            .iter()
                            .all(|scope| member_scopes.contains(scope)),
                        "{case}: {:?}",
                        admitted.claims.scopes
                    );
                }
                let caller = CallerContext::from(admitted);
                assert_eq!(caller.ceiling, ceiling, "{case}");
                assert_eq!(caller.is_admin, role == IdentityRole::SystemAdmin, "{case}");
            }
        }
    }
}

#[tokio::test]
async fn an_administrators_full_scope_is_the_member_scopes_on_the_public_surface() {
    let hub = hub().await;
    let admitted = Arrival::new(Surface::Public, None)
        .scope(admit(hub.state.door(), &hub.people[0].jwt))
        .await
        .unwrap();
    assert_eq!(
        admitted.claims.scopes,
        jwt::scopes_for_role(IdentityRole::Member)
    );
}

#[tokio::test]
async fn a_token_signed_for_one_surface_is_no_token_on_the_other() {
    let hub = hub().await;
    let admin = &hub.people[0];
    for surface in [Surface::Combined, Surface::Public] {
        let refused = Arrival::new(surface, None)
            .scope(admit(hub.state.door(), &admin.admin_jwt))
            .await;
        assert_eq!(
            refused.map(|a| a.kind),
            Err(AuthFailure::InvalidCredentials)
        );
    }
    let refused = Arrival::new(Surface::Admin, None)
        .scope(admit(hub.state.door(), &admin.jwt))
        .await;
    assert_eq!(
        refused.map(|a| a.kind),
        Err(AuthFailure::InvalidCredentials)
    );

    let config = hub.state.jwt_config.as_ref().unwrap();
    assert_eq!(
        config.surface_key(Surface::Combined).as_ref(),
        config.secret.as_bytes()
    );
    assert_eq!(
        config.surface_key(Surface::Public),
        config.surface_key(Surface::Combined)
    );
    assert_ne!(
        config.surface_key(Surface::Admin),
        config.surface_key(Surface::Combined)
    );
}

#[tokio::test]
async fn a_task_spawned_outside_every_scope_admits_as_the_default_surface() {
    let hub = hub().await;
    let (inside, spawned) = Arrival::new(Surface::Public, None)
        .scope(async {
            let inside = admit(hub.state.door(), STATIC_TOKEN).await;
            let state = Arc::clone(&hub.state);
            let spawned = tokio::spawn(async move {
                admit(state.door(), STATIC_TOKEN)
                    .await
                    .map(|admitted| admitted.ceiling)
            })
            .await
            .unwrap();
            (inside.map(|admitted| admitted.ceiling), spawned)
        })
        .await;
    assert_eq!(inside, Err(AuthFailure::SurfaceRefused));
    assert_eq!(spawned, Ok(Ceiling::Full));
}

#[tokio::test]
async fn the_owner_check_reads_the_role_its_person_holds_now() {
    let hub = hub().await;
    let owner = |token: &str| crate::gateway::check_owner_auth(&hub.state, &bearer(token), None);
    let [admin, manager, custom, member, former] = &hub.people[..] else {
        unreachable!("five people");
    };
    assert!(owner(&admin.jwt));
    assert!(!owner(&manager.jwt));
    assert!(!owner(&custom.jwt));
    assert!(!owner(&member.jwt));
    assert!(
        !owner(&former.jwt),
        "minted as an administrator, since suspended"
    );
    assert!(!owner(&admin.device), "a device is never the owner");
    assert!(owner(STATIC_TOKEN));

    let mut demoted = admin.identity.clone();
    demoted.set_role(IdentityRole::Member);
    hub.identities.update(&demoted).unwrap();
    assert!(
        !owner(&admin.jwt),
        "minted as an administrator, since demoted"
    );
}

#[tokio::test]
async fn on_the_public_surface_no_credential_is_the_owner() {
    let hub = hub().await;
    let headers = bearer(&hub.people[0].jwt);
    let (owner_by_jwt, owner_by_static_token) = Arrival::new(Surface::Public, None)
        .scope(async {
            (
                crate::gateway::check_owner_auth(&hub.state, &headers, None),
                crate::gateway::check_owner_auth(
                    &hub.state,
                    &axum::http::HeaderMap::new(),
                    Some(STATIC_TOKEN),
                ),
            )
        })
        .await;
    assert!(!owner_by_jwt);
    assert!(!owner_by_static_token);
}

#[tokio::test]
async fn a_refresh_on_the_public_surface_cannot_widen_a_token() {
    let hub = hub().await;
    let state = Arc::new(jwt::JwtAuthState {
        config: hub.state.jwt_config.clone().unwrap(),
        static_token: Some(STATIC_TOKEN.to_string()),
        device_pairing: None,
        identity_store: Some(Arc::clone(&hub.identities)),
    });
    let request = axum::extract::Request::builder()
        .uri("/api/auth/refresh")
        .method("POST")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", hub.people[0].jwt),
        )
        .body(axum::body::Body::empty())
        .unwrap();
    let refreshed = Arrival::new(Surface::Public, None)
        .scope(jwt::refresh_handler(
            axum::extract::State(Arc::clone(&state)),
            request,
        ))
        .await
        .map_err(|(status, _)| status)
        .unwrap()
        .0
        .token;
    let claims =
        jwt::validate_token(&state.config.surface_key(Surface::Public), &refreshed).unwrap();
    assert_eq!(claims.role, IdentityRole::Member);
    assert_eq!(claims.scopes, jwt::scopes_for_role(IdentityRole::Member));
}

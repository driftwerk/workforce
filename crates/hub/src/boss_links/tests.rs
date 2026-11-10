//! Desk boss links: device-only mint, list and revoke; the link token
//! reaching nothing but a link; and every way a link ends.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Duration;
use uuid::Uuid;

use workforce_common::settings::DeskBossConfig;
use workforce_identity::remote_mcp_token::{TokenScope, DESK_BOSS_TOKEN_PREFIX};
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};

use super::auth::{authenticate_link, LinkRefusal};
use super::engine_key::{fresh_nonce, EngineKey};
use super::links::{self, LinkError, MintRequest};
use crate::gateway::GatewayState;
use crate::remote_mcp::testing::{
    bearer, fixture_with_devices, pair_device, pair_device_as, Fixture,
};

struct Hub {
    fx: Fixture,
    devices: Arc<workforce_pairing::SqlitePairingStore>,
    _dirs: Vec<tempfile::TempDir>,
}

/// A hub with `[desk_boss]` as `config` says, device minting of remote MCP
/// tokens on, and two agents: `allan`, who accepts boss work, and `bea`,
/// who doesn't.
async fn hub(config: DeskBossConfig) -> Hub {
    let (fx, devices) = fixture_with_devices(true);
    let state = Arc::try_unwrap(fx.state)
        .ok()
        .expect("the fixture's only handle");
    let fx = Fixture {
        state: Arc::new(GatewayState {
            desk_boss: config,
            ..state
        }),
        identities: fx.identities,
        tokens: fx.tokens,
    };
    let accepting = tempfile::tempdir().unwrap();
    let declining = tempfile::tempdir().unwrap();
    crate::remote_mcp::testing::with_agents(&fx, accepting.path(), &["allan"]).await;
    crate::remote_mcp::testing::with_agents(&fx, declining.path(), &["bea"]).await;
    let allan = fx.state.registry.get("allan").unwrap();
    std::fs::create_dir_all(&allan.workspace).unwrap();
    std::fs::write(
        allan
            .workspace
            .join(workforce_common::config_files::WORKSPACE_CONFIG),
        "[desk_boss]\naccept = true\n",
    )
    .unwrap();
    Hub {
        fx,
        devices,
        _dirs: vec![accepting, declining],
    }
}

fn on() -> DeskBossConfig {
    DeskBossConfig {
        enabled: true,
        ..Default::default()
    }
}

impl Hub {
    fn state(&self) -> &GatewayState {
        &self.fx.state
    }

    fn person(&self, role: IdentityRole) -> Identity {
        let who = Identity::new("Sam", role);
        self.fx.identities.create(&who).unwrap();
        who
    }

    /// A person with a paired Desk: its device and bearer.
    fn sam(&self) -> (Identity, workforce_pairing::ApprovedDevice, String) {
        let mut who = self.person(IdentityRole::Member);
        let (device, bearer) = pair_device(&self.fx, &self.devices, &mut who, "Sam's Mac");
        (who, device, bearer)
    }
}

fn request(engine: &EngineKey, group: &str, engine_id: &str) -> MintRequest {
    MintRequest {
        agent_id: "allan".into(),
        group_key: format!("d-1/{group}"),
        group_name: group.into(),
        engine_id: engine_id.into(),
        engine_name: engine_id.into(),
        engine_public_key: engine.public_key().to_text(),
        renews: None,
    }
}

fn key() -> EngineKey {
    EngineKey::generate().unwrap().0
}

#[tokio::test]
async fn a_desk_mints_a_link_bound_to_its_person_device_agent_and_engine_key() {
    let hub = hub(on()).await;
    let (sam, device, _) = hub.sam();
    let engine = key();
    let minted = links::mint(
        hub.state(),
        sam.id,
        &device.id,
        &request(&engine, "acme", "devbox"),
    )
    .unwrap();

    assert!(minted.plaintext.starts_with(DESK_BOSS_TOKEN_PREFIX));
    assert!(!format!("{minted:?}").contains(&minted.plaintext));
    let record = &minted.record;
    assert_eq!(record.identity_id, sam.id);
    assert_eq!(record.scope(), Some(TokenScope::DeskBoss));
    assert_eq!(record.minted_by_device(), Some(device.id.as_str()));
    assert_eq!(record.expires_at - record.created_at, Duration::days(30));
    assert_eq!(record.label, "allan bosses acme on devbox");
    assert_eq!(minted.link.agent_id, "allan");
    assert_eq!(minted.link.engine_public_key, engine.public_key().to_text());

    let live = authenticate_link(hub.state(), &minted.plaintext).unwrap();
    assert_eq!(live.person.id, sam.id);
    assert_eq!(live.device_id, device.id);
    assert_eq!(live.agent_id, "allan");
    assert!(!format!("{live:?}").contains(&minted.plaintext));
    // The engine proves its key over a fresh challenge; another key can't.
    let nonce = fresh_nonce().unwrap();
    let proof = engine.sign_challenge(&nonce, record.id, "devbox");
    assert!(live.proves(&nonce, &proof));
    assert!(!live.proves(&nonce, &key().sign_challenge(&nonce, record.id, "devbox")));
    assert!(!live.proves(&fresh_nonce().unwrap(), &proof));

    let listed = links::list(hub.state(), sam.id, &device.id).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, record.id);
}

#[tokio::test]
async fn nothing_links_while_the_hub_is_off() {
    let hub = hub(DeskBossConfig::default()).await;
    let (sam, device, bearer_token) = hub.sam();
    assert!(matches!(
        links::mint(
            hub.state(),
            sam.id,
            &device.id,
            &request(&key(), "acme", "devbox")
        ),
        Err(LinkError::Disabled)
    ));
    assert!(matches!(
        links::list(hub.state(), sam.id, &device.id),
        Err(LinkError::Disabled)
    ));
    let (status, _) = crate::api_handlers::desk_boss_links::list(
        State(Arc::clone(&hub.fx.state)),
        bearer(&bearer_token),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_person_the_agent_and_what_desk_sent_are_all_checked() {
    let hub = hub(on()).await;
    let (sam, device, _) = hub.sam();
    let engine = key();
    let mint = |req: &MintRequest| links::mint(hub.state(), sam.id, &device.id, req);

    let declines = MintRequest {
        agent_id: "bea".into(),
        ..request(&engine, "acme", "devbox")
    };
    assert!(matches!(mint(&declines), Err(LinkError::AgentDeclines)));
    for unknown in ["nobody", "", "  "] {
        let req = MintRequest {
            agent_id: unknown.into(),
            ..request(&engine, "acme", "devbox")
        };
        assert!(
            matches!(mint(&req), Err(LinkError::AgentUnreachable)),
            "{unknown:?}"
        );
    }
    // An agent whose access policy leaves Sam out reads as one that isn't.
    hub.fx.state.access_gate.upsert(
        "allan".into(),
        workforce_common::settings::AgentAccessConfig {
            mode: workforce_common::settings::AccessMode::Allowlist,
            allow: vec!["someone-else".into()],
            ..Default::default()
        },
    );
    assert!(matches!(
        mint(&request(&engine, "acme", "devbox")),
        Err(LinkError::AgentUnreachable)
    ));
    hub.fx
        .state
        .access_gate
        .upsert("allan".into(), Default::default());

    for bad in [
        MintRequest {
            engine_public_key: "ed25519:short".into(),
            ..request(&engine, "acme", "devbox")
        },
        MintRequest {
            engine_public_key: format!("ed25519:{}", "A".repeat(43)),
            ..request(&engine, "acme", "devbox")
        },
    ] {
        assert!(matches!(mint(&bad), Err(LinkError::BadKey(_))));
    }
    for bad in [
        request(&engine, "acme", "dev box"),
        request(&engine, "acme", ""),
        request(&engine, "acme", &"e".repeat(65)),
        MintRequest {
            group_name: "Ac\nme".into(),
            ..request(&engine, "acme", "devbox")
        },
        MintRequest {
            group_key: " ".into(),
            ..request(&engine, "acme", "devbox")
        },
        MintRequest {
            engine_name: "x".repeat(65),
            ..request(&engine, "acme", "devbox")
        },
    ] {
        assert!(matches!(mint(&bad), Err(LinkError::Invalid(_))), "{bad:?}");
    }

    let mut suspended = hub.person(IdentityRole::Member);
    let (theirs, _) = pair_device(&hub.fx, &hub.devices, &mut suspended, "Mac");
    suspended.status = IdentityStatus::Suspended;
    hub.fx.identities.update(&suspended).unwrap();
    assert!(matches!(
        links::mint(
            hub.state(),
            suspended.id,
            &theirs.id,
            &request(&engine, "acme", "devbox")
        ),
        Err(LinkError::IdentityNotActive)
    ));
    assert!(hub.fx.tokens.list_all_tokens().unwrap().is_empty());
}

#[tokio::test]
async fn a_device_holds_one_link_per_triple_and_no_more_than_its_cap() {
    let hub = hub(DeskBossConfig {
        max_links_per_device: 2,
        ..on()
    })
    .await;
    let (sam, device, _) = hub.sam();
    let engine = key();
    let mint = |group: &str, engine_id: &str| {
        links::mint(
            hub.state(),
            sam.id,
            &device.id,
            &request(&engine, group, engine_id),
        )
    };
    let first = mint("acme", "devbox").unwrap();
    // The same (agent, group, engine) again replaces it.
    let again = mint("acme", "devbox").unwrap();
    assert_ne!(first.link.link_id, again.link.link_id);
    assert_eq!(
        authenticate_link(hub.state(), &first.plaintext).err(),
        Some(LinkRefusal::Unauthenticated)
    );
    assert!(authenticate_link(hub.state(), &again.plaintext).is_ok());
    mint("acme", "mac").unwrap();
    assert!(matches!(
        mint("atlas", "devbox"),
        Err(LinkError::LinkCap(2))
    ));
    // Replacing one of the two is no new link.
    assert!(mint("acme", "mac").is_ok());
    assert_eq!(
        links::list(hub.state(), sam.id, &device.id).unwrap().len(),
        2
    );
    // Another Desk of Sam's has a cap of its own.
    let mut same_sam = hub.fx.identities.get_by_id(sam.id).unwrap().unwrap();
    let (laptop, _) = pair_device(&hub.fx, &hub.devices, &mut same_sam, "Sam's laptop");
    assert!(links::mint(
        hub.state(),
        sam.id,
        &laptop.id,
        &request(&engine, "atlas", "devbox")
    )
    .is_ok());
}

#[tokio::test]
async fn a_renewal_keeps_the_link_and_never_leaves_more_than_two_tokens() {
    let hub = hub(DeskBossConfig {
        max_links_per_device: 1,
        ..on()
    })
    .await;
    let (sam, device, _) = hub.sam();
    let engine = key();
    let renew = |renews: Option<Uuid>| {
        links::mint(
            hub.state(),
            sam.id,
            &device.id,
            &MintRequest {
                renews,
                ..request(&engine, "acme", "devbox")
            },
        )
    };
    let first = renew(None).unwrap();
    // At the cap, a renewal still goes: it is the same link.
    let second = renew(Some(first.record.id)).unwrap();
    assert_eq!(second.link.link_id, first.link.link_id);
    assert!(authenticate_link(hub.state(), &first.plaintext).is_ok());
    assert!(authenticate_link(hub.state(), &second.plaintext).is_ok());
    let third = renew(Some(second.record.id)).unwrap();
    assert_eq!(third.link.link_id, first.link.link_id);
    assert_eq!(
        authenticate_link(hub.state(), &first.plaintext).err(),
        Some(LinkRefusal::Unauthenticated)
    );
    assert!(authenticate_link(hub.state(), &second.plaintext).is_ok());
    assert_eq!(
        links::list(hub.state(), sam.id, &device.id).unwrap().len(),
        2
    );

    assert!(matches!(
        renew(Some(Uuid::new_v4())),
        Err(LinkError::RenewsUnknown)
    ));
    // A renewal for another group, engine or agent is no renewal.
    let other_group = MintRequest {
        renews: Some(third.record.id),
        ..request(&engine, "atlas", "devbox")
    };
    assert!(matches!(
        links::mint(hub.state(), sam.id, &device.id, &other_group),
        Err(LinkError::RenewsUnknown)
    ));
    // Nor is one from another device of Sam's.
    let mut same_sam = hub.fx.identities.get_by_id(sam.id).unwrap().unwrap();
    let (laptop, _) = pair_device(&hub.fx, &hub.devices, &mut same_sam, "laptop");
    assert!(matches!(
        links::mint(
            hub.state(),
            sam.id,
            &laptop.id,
            &MintRequest {
                renews: Some(third.record.id),
                ..request(&engine, "acme", "devbox")
            }
        ),
        Err(LinkError::RenewsUnknown)
    ));
}

#[tokio::test]
async fn a_desk_sees_and_revokes_only_its_own_links() {
    let hub = hub(on()).await;
    let (sam, device, _) = hub.sam();
    let engine = key();
    let mine = links::mint(
        hub.state(),
        sam.id,
        &device.id,
        &request(&engine, "acme", "devbox"),
    )
    .unwrap();
    let mut same_sam = hub.fx.identities.get_by_id(sam.id).unwrap().unwrap();
    let (laptop, _) = pair_device(&hub.fx, &hub.devices, &mut same_sam, "laptop");
    assert!(links::list(hub.state(), sam.id, &laptop.id)
        .unwrap()
        .is_empty());
    assert!(!links::revoke(hub.state(), sam.id, &laptop.id, mine.record.id).unwrap());
    let other = hub.person(IdentityRole::Member);
    assert!(!links::revoke(hub.state(), other.id, &device.id, mine.record.id).unwrap());
    assert!(!links::revoke(hub.state(), sam.id, &device.id, Uuid::new_v4()).unwrap());
    // The remote MCP device route never revokes a boss link.
    let mcp = crate::remote_mcp::tokens::RemoteMcpTokenService::for_gateway(hub.state()).unwrap();
    assert!(!mcp
        .revoke_for_device(sam.id, &device.id, mine.record.id)
        .unwrap());
    assert!(authenticate_link(hub.state(), &mine.plaintext).is_ok());

    assert!(links::revoke(hub.state(), sam.id, &device.id, mine.record.id).unwrap());
    assert_eq!(
        authenticate_link(hub.state(), &mine.plaintext).err(),
        Some(LinkRefusal::Unauthenticated)
    );
    assert!(!links::revoke(hub.state(), sam.id, &device.id, mine.record.id).unwrap());
}

/// The device's remote MCP token and its boss links live side by side:
/// minting the one never revokes, nor counts against, the other.
#[tokio::test]
async fn remote_mcp_device_tokens_leave_boss_links_alone() {
    let hub = hub(on()).await;
    let (sam, device, _) = hub.sam();
    let link = links::mint(
        hub.state(),
        sam.id,
        &device.id,
        &request(&key(), "acme", "devbox"),
    )
    .unwrap();
    let mcp = crate::remote_mcp::tokens::RemoteMcpTokenService::for_gateway(hub.state()).unwrap();
    for _ in 0..3 {
        mcp.mint_for_device(sam.id, &device.id, "Sam's Mac")
            .unwrap();
    }
    assert!(authenticate_link(hub.state(), &link.plaintext).is_ok());
    let kinds: Vec<_> = mcp
        .list(sam.id)
        .unwrap()
        .iter()
        .map(|t| t.scope())
        .collect();
    assert_eq!(kinds.len(), 2);
    assert!(kinds.contains(&Some(TokenScope::DeskBoss)));
    assert!(kinds.contains(&Some(TokenScope::RemoteMcp)));
}

#[tokio::test]
async fn unpairing_the_device_ends_its_links() {
    let hub = hub(on()).await;
    let (sam, device, _) = hub.sam();
    let a = links::mint(
        hub.state(),
        sam.id,
        &device.id,
        &request(&key(), "acme", "devbox"),
    )
    .unwrap();
    let b = links::mint(
        hub.state(),
        sam.id,
        &device.id,
        &request(&key(), "acme", "mac"),
    )
    .unwrap();
    // Revoked in the pairing store alone, a link is refused at once (and
    // revoked where it is found).
    workforce_pairing::PairingStore::revoke_device(hub.devices.as_ref(), &device.id).unwrap();
    assert_eq!(
        authenticate_link(hub.state(), &a.plaintext).err(),
        Some(LinkRefusal::Unauthenticated)
    );
    // And the unpair paths' sweep revokes every link the device made.
    assert_eq!(
        crate::remote_mcp::tokens::revoke_device_tokens(hub.fx.tokens.as_ref(), &device.id),
        1
    );
    assert_eq!(
        authenticate_link(hub.state(), &b.plaintext).err(),
        Some(LinkRefusal::Unauthenticated)
    );
    assert!(hub
        .fx
        .tokens
        .list_tokens_for_identity(sam.id)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_link_ends_when_its_person_or_agent_does() {
    let hub = hub(on()).await;
    let (mut sam, device, _) = hub.sam();
    let link = links::mint(
        hub.state(),
        sam.id,
        &device.id,
        &request(&key(), "acme", "devbox"),
    )
    .unwrap();
    // The agent turning boss work off ends the link at once.
    let allan = hub.fx.state.registry.get("allan").unwrap();
    let config = allan
        .workspace
        .join(workforce_common::config_files::WORKSPACE_CONFIG);
    std::fs::write(&config, "[desk_boss]\naccept = false\n").unwrap();
    assert_eq!(
        authenticate_link(hub.state(), &link.plaintext).err(),
        Some(LinkRefusal::AgentDeclines)
    );
    std::fs::write(&config, "[desk_boss]\naccept = true\n").unwrap();
    assert!(authenticate_link(hub.state(), &link.plaintext).is_ok());
    sam.status = IdentityStatus::Suspended;
    hub.fx.identities.update(&sam).unwrap();
    assert_eq!(
        authenticate_link(hub.state(), &link.plaintext).err(),
        Some(LinkRefusal::NotActive)
    );
}

/// A link token carries a boss link and nothing else: no remote MCP, no
/// REST identity, no device route, no admin.
#[tokio::test]
async fn a_link_token_reaches_nothing_but_its_link() {
    let hub = hub(on()).await;
    let (sam, device, _) = hub.sam();
    let link = links::mint(
        hub.state(),
        sam.id,
        &device.id,
        &request(&key(), "acme", "devbox"),
    )
    .unwrap();
    let headers = bearer(&link.plaintext);

    assert_eq!(
        crate::remote_mcp::auth::authenticate(hub.state(), &headers).err(),
        Some(crate::remote_mcp::auth::AuthError::Unauthenticated)
    );
    let caller = crate::gateway::resolve_caller_context(hub.state(), &headers, None).await;
    assert!(caller.identity_id.is_none());
    assert!(caller.device_id.is_none());
    assert!(!caller.is_admin);
    assert!(caller.scopes.is_empty());
    assert!(crate::api_handlers::identity::require_auth(hub.state(), &headers).is_err());
    // The device-only routes take a device's own token, never a link's.
    let refused = crate::api_handlers::desk_boss_links::list(
        State(Arc::clone(&hub.fx.state)),
        headers.clone(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(refused.0, StatusCode::FORBIDDEN);
    let refused = crate::api_handlers::remote_mcp_tokens::create_device_token(
        State(Arc::clone(&hub.fx.state)),
        headers.clone(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(refused.0, StatusCode::FORBIDDEN);
    // A remote MCP token is no link, nor is a device's own token.
    let mcp = crate::remote_mcp::tokens::RemoteMcpTokenService::for_gateway(hub.state())
        .unwrap()
        .mint(sam.id, "laptop")
        .unwrap();
    assert_eq!(
        authenticate_link(hub.state(), &mcp.plaintext).err(),
        Some(LinkRefusal::Unauthenticated)
    );
    for not_a_link in ["", "wfb_", "wfp_device", &link.plaintext[1..]] {
        assert_eq!(
            authenticate_link(hub.state(), not_a_link).err(),
            Some(LinkRefusal::Unauthenticated),
            "{not_a_link:?}"
        );
    }
    // A `desk_boss` row without its link is no link either.
    let stripped = workforce_identity::remote_mcp_token::mint_as(TokenScope::DeskBoss);
    hub.fx
        .tokens
        .create_token(&workforce_identity::remote_mcp_token::RemoteMcpToken {
            id: Uuid::new_v4(),
            token_hash: stripped.hash,
            desk_link: None,
            ..link.record.clone()
        })
        .unwrap();
    assert_eq!(
        authenticate_link(hub.state(), &stripped.plaintext).err(),
        Some(LinkRefusal::Unauthenticated)
    );
}

/// The routes: a phone, a login or the hub's own token can't mint; the
/// token is answered once and never listed.
#[tokio::test]
async fn the_routes_are_a_paired_desks_alone() {
    let hub = hub(on()).await;
    let (sam, _device, desk) = hub.sam();
    let engine = key();
    let body = |renews: Option<Uuid>| {
        serde_json::from_value::<crate::api_handlers::desk_boss_links::MintBody>(serde_json::json!({
            "agent_id": "allan",
            "group": {"key": "d-1/acme", "name": "Acme"},
            "engine": {"id": "devbox", "name": "devbox", "public_key": engine.public_key().to_text()},
            "renews": renews,
        }))
        .unwrap()
    };
    let create = |bearer_token: String, renews: Option<Uuid>| {
        let state = Arc::clone(&hub.fx.state);
        let body = body(renews);
        async move {
            crate::api_handlers::desk_boss_links::create(
                State(state),
                bearer(&bearer_token),
                Json(body),
            )
            .await
            .map(|(status, Json(v))| (status, serde_json::to_value(v).unwrap()))
            .map_err(|(status, _)| status)
        }
    };
    let mut phone_owner = hub.fx.identities.get_by_id(sam.id).unwrap().unwrap();
    let (_, phone) = pair_device_as(
        &hub.fx,
        &hub.devices,
        &mut phone_owner,
        "Sam's phone",
        "ios_device",
    );
    for refused in [
        phone,
        "gateway-token".to_string(),
        "not-a-token".to_string(),
    ] {
        assert_eq!(
            create(refused.clone(), None).await.err(),
            Some(StatusCode::FORBIDDEN),
            "{refused}"
        );
    }
    let (status, minted) = create(desk.clone(), None).await.unwrap();
    assert_eq!(status, StatusCode::CREATED);
    let token = minted["token"].as_str().unwrap().to_string();
    assert!(token.starts_with(DESK_BOSS_TOKEN_PREFIX));
    let id: Uuid = minted["id"].as_str().unwrap().parse().unwrap();

    let Json(listed) =
        crate::api_handlers::desk_boss_links::list(State(Arc::clone(&hub.fx.state)), bearer(&desk))
            .await
            .unwrap();
    let listed = serde_json::to_value(listed).unwrap();
    assert_eq!(listed["links"][0]["group_name"], "Acme");
    assert_eq!(listed["links"][0]["link_id"], minted["link_id"]);
    let text = listed.to_string();
    let body_part = &token[DESK_BOSS_TOKEN_PREFIX.len()..];
    for start in 0..=body_part.len() - 6 {
        assert!(
            !text.contains(&body_part[start..start + 6]),
            "the listing leaked part of the token"
        );
    }

    let revoke = |bearer_token: String, id: String| {
        let state = Arc::clone(&hub.fx.state);
        async move {
            crate::api_handlers::desk_boss_links::revoke(
                State(state),
                bearer(&bearer_token),
                Path(id),
            )
            .await
            .map_err(|(status, _)| status)
        }
    };
    assert_eq!(
        revoke(desk.clone(), "nope".into()).await.err(),
        Some(StatusCode::NOT_FOUND)
    );
    assert_eq!(
        revoke(token.clone(), id.to_string()).await.err(),
        Some(StatusCode::FORBIDDEN)
    );
    assert_eq!(
        revoke(desk.clone(), id.to_string()).await,
        Ok(StatusCode::NO_CONTENT)
    );
    assert_eq!(
        revoke(desk.clone(), id.to_string()).await.err(),
        Some(StatusCode::NOT_FOUND)
    );
}

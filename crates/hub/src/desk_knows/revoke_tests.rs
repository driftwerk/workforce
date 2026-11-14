//! A Desk work token ends itself (unlinking an agent) or is ended by the
//! device that minted it for a removed engine (removing a machine), and
//! nothing else is ended with it.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{Duration, Utc};
use uuid::Uuid;

use workforce_identity::remote_mcp_token::{
    self, DeskLink, RemoteMcpToken, TokenScope, DEVICE_SCOPE_PREFIX,
};
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};

use super::routes::{revoke_engine, revoke_self};
use crate::remote_mcp::testing::{bearer, fixture_with_devices, pair_device, Fixture};

/// Stores a Desk work token of `person` minted by `device` for `agent`
/// under `engine`, and returns its id and plaintext.
fn mint(fx: &Fixture, person: Uuid, device: &str, agent: &str, engine: &str) -> (Uuid, String) {
    let minted = remote_mcp_token::mint_as(TokenScope::DeskKnows);
    let now = Utc::now();
    let id = Uuid::new_v4();
    fx.tokens
        .create_token(&RemoteMcpToken {
            id,
            identity_id: person,
            label: "Desk work".into(),
            token_hash: minted.hash,
            scopes: vec![
                TokenScope::DeskKnows.key().to_string(),
                format!("{DEVICE_SCOPE_PREFIX}{device}"),
            ],
            created_at: now,
            expires_at: now + Duration::days(30),
            last_used_at: None,
            revoked_at: None,
            desk_link: Some(DeskLink {
                link_id: Uuid::new_v4(),
                agent_id: agent.into(),
                group_key: String::new(),
                desk_group_key: String::new(),
                group_name: String::new(),
                engine_id: engine.into(),
                engine_name: "Desk".into(),
                engine_public_key: String::new(),
            }),
        })
        .unwrap();
    (id, minted.plaintext)
}

fn live(fx: &Fixture, person: Uuid, id: Uuid) -> bool {
    fx.tokens
        .list_tokens_for_identity(person)
        .unwrap()
        .iter()
        .find(|t| t.id == id)
        .is_some_and(|t| t.is_live(Utc::now()))
}

#[tokio::test]
async fn a_token_ends_itself_and_nothing_else_with_it() {
    let (fx, devices) = fixture_with_devices(true);
    let mut sam = Identity::new("Sam", IdentityRole::Member);
    fx.identities.create(&sam).unwrap();
    let (device, _) = pair_device(&fx, &devices, &mut sam, "Mac");
    let (one, plain_one) = mint(&fx, sam.id, &device.id, "allan", "local");
    let (two, _) = mint(&fx, sam.id, &device.id, "allan", "devbox");

    let state = || State(Arc::clone(&fx.state));
    let ended = revoke_self(state(), bearer(&plain_one)).await;
    assert_eq!(ended.status(), StatusCode::NO_CONTENT);
    assert!(!live(&fx, sam.id, one));
    assert!(live(&fx, sam.id, two));
    // Asked again, the hub no longer knows it: the Desk that held it is done.
    let again = revoke_self(state(), bearer(&plain_one)).await;
    assert_eq!(again.status(), StatusCode::UNAUTHORIZED);
    // A token of another kind, or none, ends nothing.
    let other = remote_mcp_token::mint();
    for headers in [bearer(&other.plaintext), axum::http::HeaderMap::new()] {
        let refused = revoke_self(state(), headers).await;
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(live(&fx, sam.id, two));
}

#[tokio::test]
async fn a_removed_engines_tokens_are_ended_by_the_device_that_minted_them_alone() {
    let (fx, devices) = fixture_with_devices(true);
    let mut sam = Identity::new("Sam", IdentityRole::Member);
    fx.identities.create(&sam).unwrap();
    let (mac, mac_bearer) = pair_device(&fx, &devices, &mut sam, "Mac");
    let (laptop, _) = pair_device(&fx, &devices, &mut sam, "Laptop");
    let mut kim = Identity::new("Kim", IdentityRole::Member);
    fx.identities.create(&kim).unwrap();
    let (kim_mac, kim_bearer) = pair_device(&fx, &devices, &mut kim, "Kim's Mac");

    let (devbox_allan, _) = mint(&fx, sam.id, &mac.id, "allan", "devbox");
    let (devbox_bea, _) = mint(&fx, sam.id, &mac.id, "bea", "devbox");
    let (home, _) = mint(&fx, sam.id, &mac.id, "allan", "local");
    let (other_machine, _) = mint(&fx, sam.id, &mac.id, "allan", "nas");
    let (laptops, _) = mint(&fx, sam.id, &laptop.id, "allan", "devbox");
    let (kims, _) = mint(&fx, kim.id, &kim_mac.id, "allan", "devbox");

    let call = |headers, engine: &str| {
        revoke_engine(State(Arc::clone(&fx.state)), headers, Path(engine.to_string()))
    };
    let done = call(bearer(&mac_bearer), "devbox").await;
    assert_eq!(done.status(), StatusCode::OK);
    assert!(!live(&fx, sam.id, devbox_allan));
    assert!(!live(&fx, sam.id, devbox_bea));
    // This computer's own, another machine's, another device's and another
    // person's stay.
    assert!(live(&fx, sam.id, home));
    assert!(live(&fx, sam.id, other_machine));
    assert!(live(&fx, sam.id, laptops));
    assert!(live(&fx, kim.id, kims));
    // Kim's device names the same engine and reaches only Kim's.
    assert_eq!(call(bearer(&kim_bearer), "devbox").await.status(), StatusCode::OK);
    assert!(!live(&fx, kim.id, kims));
    assert!(live(&fx, sam.id, laptops));
    // Only a paired device asks; an engine is named plainly.
    assert_eq!(
        call(axum::http::HeaderMap::new(), "nas").await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(bearer(&mac_bearer), "a b").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert!(live(&fx, sam.id, other_machine));
}

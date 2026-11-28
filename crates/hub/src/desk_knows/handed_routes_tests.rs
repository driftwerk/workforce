//! A request's take and words back, by the person's own computer's Desk
//! alone, on their own requests, within its cap, told only where it was
//! asked.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::Response;
use chrono::Utc;
use uuid::Uuid;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};

use super::revoke_tests::mint;
use super::routes::{saying, taking};
use super::store::{tests::handed, KnowsStore, MemoryKnows, Reader, MAX_SAID};
use crate::remote_mcp::testing::{bearer, fixture_with_devices, pair_device, with_agents};

async fn body(r: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or_default()
}

#[tokio::test]
async fn only_the_persons_own_computers_desk_takes_and_answers_their_own_requests_where_asked() {
    let dir = tempfile::tempdir().unwrap();
    let (fx, devices) = fixture_with_devices(true);
    with_agents(&fx, dir.path(), &["allan"]).await;
    let service = fx.state.registry.get("allan").unwrap();
    let slack = crate::serve::testing::SentToSlack::on(&service).await;
    let mut sam = Identity::new("Sam", IdentityRole::Member);
    fx.identities.create(&sam).unwrap();
    let mut kim = Identity::new("Kim", IdentityRole::Member);
    fx.identities.create(&kim).unwrap();
    let (device, _) = pair_device(&fx, &devices, &mut sam, "Mac");
    let (kims_device, _) = pair_device(&fx, &devices, &mut kim, "Mac");
    let (_, mac) = mint(&fx, sam.id, &device.id, "allan", "local");
    let (_, machine) = mint(&fx, sam.id, &device.id, "allan", "devbox");
    let (_, kims) = mint(&fx, kim.id, &kims_device.id, "allan", "local");

    let knows = MemoryKnows::new();
    let store = || Ok(Arc::clone(&knows) as Arc<dyn KnowsStore>);
    let h = handed(sam.id, "keep the video on track", Utc::now());
    knows.hand(&h).await.unwrap();
    let say = serde_json::json!({ "text": "Your Desk boss has it." }).to_string();

    let r = taking(&fx.state, &bearer(&machine), b"", store).await;
    assert_eq!(
        r.status(),
        StatusCode::FORBIDDEN,
        "a machine's Desk takes nothing"
    );
    let r = saying(&fx.state, h.id, &bearer(&machine), say.as_bytes(), store).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "nor answers");

    let r = taking(&fx.state, &bearer(&kims), b"", store).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(body(r).await["handed"], serde_json::json!([]), "another's");

    let r = taking(&fx.state, &bearer(&mac), br#"{"delivering": []}"#, store).await;
    assert_eq!(r.status(), StatusCode::OK);
    let taken = body(r).await;
    assert_eq!(taken["handed"][0]["id"], h.id.to_string());
    assert_eq!(taken["handed"][0]["platform"], "slack");
    assert!(taken["handed"][0].get("agentName").is_none());
    let r = taking(&fx.state, &bearer(&mac), b"not json", store).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    let r = saying(&fx.state, h.id, &bearer(&kims), say.as_bytes(), store).await;
    assert_eq!(
        r.status(),
        StatusCode::NOT_FOUND,
        "another person's request"
    );
    let r = saying(
        &fx.state,
        Uuid::new_v4(),
        &bearer(&mac),
        say.as_bytes(),
        store,
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert!(slack.sent().is_empty());

    let r = saying(&fx.state, h.id, &bearer(&mac), say.as_bytes(), store).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        slack.sent(),
        [(
            "D1".to_string(),
            Some("1700.1".to_string()),
            "Your Desk boss has it.".to_string()
        )],
        "only where it was asked"
    );
    let reader = Reader {
        person: sam.id,
        agent: "allan".into(),
    };
    for _ in 1..MAX_SAID {
        knows.handed_heard(&reader, h.id, Utc::now()).await.unwrap();
    }
    let r = saying(&fx.state, h.id, &bearer(&mac), say.as_bytes(), store).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS, "at its cap");
    assert_eq!(slack.sent().len(), 1);
}

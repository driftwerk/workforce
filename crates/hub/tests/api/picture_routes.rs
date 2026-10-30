//! Agents' and people's pictures over the API: who may see and change them,
//! what an image is answered with, and how lists carry them.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use workforce_hub::agent_registry::AgentRegistry;
use workforce_hub::gateway::{router, GatewayState};
use workforce_hub::pictures::postgres::PostgresPictures;
use workforce_hub::pictures::{Image, Owner, PictureStore, SlackPicture};
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};

use crate::support::{agent_service, token_for, GatewayParts, GATEWAY_TOKEN};

/// The smallest PNG header the hub takes as one.
fn png(seed: u8) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend([seed; 32]);
    bytes
}

struct Hub {
    state: Arc<GatewayState>,
    store: Arc<dyn PictureStore>,
    member: Identity,
    _data: tempfile::TempDir,
}

async fn hub(pool: workforce_postgres::PostgresPool) -> Hub {
    let data = tempfile::tempdir().unwrap();
    let store: Arc<dyn PictureStore> = Arc::new(PostgresPictures::with_pool(pool).await.unwrap());
    let identities = SqliteIdentityStore::in_memory().unwrap();
    let member = Identity::new("Sam", IdentityRole::Member);
    identities.create(&member).unwrap();
    let registry = AgentRegistry::from_parts(
        [("scout".to_string(), agent_service(data.path()).await)].into(),
        "scout".to_string(),
    );
    let state = GatewayParts {
        pictures: Some(Arc::clone(&store)),
        ..GatewayParts::new(
            registry,
            Arc::new(identities) as Arc<dyn IdentityStore>,
            data.path().join("skills"),
        )
    }
    .state();
    Hub {
        state,
        store,
        member,
        _data: data,
    }
}

async fn call(
    state: &Arc<GatewayState>,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    extra: &[(header::HeaderName, &str)],
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    for (name, value) in extra {
        request = request.header(name, *value);
    }
    let response = router(Arc::clone(state))
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, bytes.to_vec())
}

fn json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

async fn with_hub<F, Fut>(test: F)
where
    F: FnOnce(Hub) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    workforce_postgres::test_support::with_test_pool(
        PostgresPictures::build_pool,
        |pool| async move {
            test(hub(pool).await).await;
        },
    )
    .await;
}

#[tokio::test]
async fn an_agents_picture_is_chosen_by_whoever_may_change_it_and_seen_by_whoever_sees_it() {
    with_hub(|hub| async move {
        let member = token_for(&hub.member);
        let path = "/api/agents/scout/picture";

        let (status, _, _) = call(&hub.state, "GET", path, None, &[], vec![]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = call(&hub.state, "GET", path, Some("wfp_dead"), &[], vec![]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = call(&hub.state, "GET", path, Some(&member), &[], vec![]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "no picture yet");

        let (status, _, _) = call(&hub.state, "PUT", path, Some(&member), &[], png(1)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a member can't change the agent"
        );
        let (status, _, body) = call(
            &hub.state,
            "PUT",
            path,
            Some(GATEWAY_TOKEN),
            &[],
            b"GIF89a....".to_vec(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{}",
            json(&body)
        );
        let mut huge = png(2);
        huge.resize(workforce_hub::pictures::MAX_BYTES + 1, 0);
        let (status, _, _) = call(&hub.state, "PUT", path, Some(GATEWAY_TOKEN), &[], huge).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        let (status, _, _) = call(
            &hub.state,
            "PUT",
            "/api/agents/nobody/picture",
            Some(GATEWAY_TOKEN),
            &[],
            png(1),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _, body) =
            call(&hub.state, "PUT", path, Some(GATEWAY_TOKEN), &[], png(1)).await;
        assert_eq!(status, StatusCode::OK);
        let chosen = json(&body)["picture"].clone();
        assert_eq!(chosen["source"], "chosen");
        let version = chosen["version"].as_str().unwrap().to_string();

        let (status, headers, bytes) =
            call(&hub.state, "GET", path, Some(&member), &[], vec![]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, png(1));
        assert_eq!(headers[header::CONTENT_TYPE], "image/png");
        assert_eq!(headers[header::ETAG], format!("\"{version}\"").as_str());
        assert_eq!(headers[header::CACHE_CONTROL], "private, no-cache");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");

        let etag = format!("\"{version}\"");
        let (status, headers, bytes) = call(
            &hub.state,
            "GET",
            path,
            Some(&member),
            &[(header::IF_NONE_MATCH, &etag)],
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert!(bytes.is_empty());
        assert_eq!(headers[header::ETAG], etag.as_str());
        let (status, _, _) = call(
            &hub.state,
            "GET",
            path,
            Some(&member),
            &[(header::IF_NONE_MATCH, "\"other\"")],
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (_, _, body) = call(&hub.state, "GET", "/api/agents", Some(&member), &[], vec![]).await;
        assert_eq!(json(&body)["agents"][0]["picture"], chosen);

        let (status, _, body) = call(&hub.state, "DELETE", path, Some(&member), &[], vec![]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{}", json(&body));
        let (status, _, body) =
            call(&hub.state, "DELETE", path, Some(GATEWAY_TOKEN), &[], vec![]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json(&body)["picture"], Value::Null);
        let (_, _, body) = call(&hub.state, "GET", "/api/agents", Some(&member), &[], vec![]).await;
        assert!(json(&body)["agents"][0].get("picture").is_none());
    })
    .await;
}

#[tokio::test]
async fn removing_a_chosen_picture_brings_back_the_slack_one() {
    with_hub(|hub| async move {
        let slack = SlackPicture {
            image: Image::accept(png(7)).unwrap(),
            origin: "member:acme:U1".into(),
            workspace: "acme".into(),
        };
        hub.store
            .put_slack(&Owner::agent("scout"), &slack)
            .await
            .unwrap();
        let path = "/api/agents/scout/picture";
        let (_, _, body) = call(&hub.state, "PUT", path, Some(GATEWAY_TOKEN), &[], png(1)).await;
        assert_eq!(json(&body)["picture"]["source"], "chosen");
        assert_eq!(
            json(&body)["picture"]["slack"],
            "acme",
            "the Slack picture it overrides"
        );

        let (_, _, body) = call(&hub.state, "DELETE", path, Some(GATEWAY_TOKEN), &[], vec![]).await;
        assert_eq!(json(&body)["picture"]["source"], "slack");
        assert_eq!(json(&body)["picture"]["version"], slack.image.etag.as_str());
        let (_, _, bytes) = call(&hub.state, "GET", path, Some(GATEWAY_TOKEN), &[], vec![]).await;
        assert_eq!(bytes, png(7));
    })
    .await;
}

#[tokio::test]
async fn your_own_picture_is_yours_alone() {
    with_hub(|hub| async move {
        let member = token_for(&hub.member);
        let path = "/api/me/picture";

        let (status, _, _) = call(&hub.state, "PUT", path, Some(GATEWAY_TOKEN), &[], png(3)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the owner token is nobody"
        );
        let (status, _, _) = call(&hub.state, "GET", path, None, &[], vec![]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _, body) = call(&hub.state, "PUT", path, Some(&member), &[], png(3)).await;
        assert_eq!(status, StatusCode::OK);
        let picture = json(&body)["picture"].clone();
        assert_eq!(picture["source"], "chosen");

        let (_, _, bytes) = call(&hub.state, "GET", path, Some(&member), &[], vec![]).await;
        assert_eq!(bytes, png(3));
        let (_, _, body) = call(&hub.state, "GET", "/api/me", Some(&member), &[], vec![]).await;
        assert_eq!(json(&body)["picture"], picture);
        assert!(
            hub.store
                .shown(&Owner::identity(hub.member.id.to_string()))
                .await
                .unwrap()
                .is_some(),
            "kept on the person's identity"
        );

        let (status, _, body) = call(&hub.state, "DELETE", path, Some(&member), &[], vec![]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json(&body)["picture"], Value::Null);
        let (status, _, _) = call(&hub.state, "GET", path, Some(&member), &[], vec![]).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    })
    .await;
}

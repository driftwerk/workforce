//! A session's turns run as its owner: whoever may see a session may stop
//! its turn, and only the owner may run one in it.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use workforce_identity::types::{Identity, IdentityRole};

use workforce_hub::gateway::router;

use crate::support::{agent_service, send, token_for, Gateway, GATEWAY_TOKEN};

fn post(path: &str, bearer: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn an_administrator_stops_but_never_runs_a_turn_in_anothers_session() {
    let data = tempfile::tempdir().unwrap();
    let service = agent_service(data.path()).await;
    let gateway = Gateway::new(vec![("default", service.clone())], None);
    let alice = Identity::new("Alice", IdentityRole::Member);
    let bob = Identity::new("Bob", IdentityRole::Member);
    let mut session = service
        .sessions
        .get_or_create("rest", &alice.id.to_string(), "default")
        .unwrap();
    let message_id = session.add_user_message("what is in my notes?", None);
    session.add_assistant_message("three things", 0);
    service.sessions.save(&session).unwrap();
    let call = |action: String, bearer: String| {
        let app = router(gateway.state.clone());
        async move { send(app, post(&action, &bearer)).await }
    };
    let retry = format!("/api/sessions/{}/messages/{message_id}/retry", session.id);
    let stop = format!("/api/sessions/{}/stop", session.id);

    for admin in [gateway.admin_token(), GATEWAY_TOKEN.to_string()] {
        let (status, _, text) = call(retry.clone(), admin.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        let (status, _, text) = call(stop.clone(), admin).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(
            text.contains("\"stopped\":false"),
            "no turn was live: {text}"
        );
    }
    for action in [&retry, &stop] {
        let (status, _, text) = call(action.clone(), token_for(&bob)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{action}: {text}");
    }
    let (status, _, text) = call(stop, token_for(&alice)).await;
    assert_eq!(status, StatusCode::OK, "the owner stops their own: {text}");

    let kept = service.sessions.get(&session.id).unwrap().unwrap();
    assert_eq!(
        kept.messages.len(),
        session.messages.len(),
        "no refused retry truncated the owner's history"
    );
}

//! `POST /api/evals/grade`, `POST /api/evals/know-how` and the turn naming
//! of `POST /api/chat`: who may call them and what they refuse, on the paths
//! that need no model.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use workforce_hub::gateway::router;
use workforce_identity::types::{Identity, IdentityRole};

use crate::support::{agent_service, send, token_for, Gateway, GATEWAY_TOKEN};

const KNOW_HOW: &str = "/api/evals/know-how";

fn grade_body(answer: &str) -> Value {
    json!({
        "question": "How many orders shipped last week?",
        "expected": "42",
        "rubric": {
            "points": [{"id": "count", "text": "Gives 42"}],
            "correct": "The point is met.",
            "partly": "The method is right and the number is not.",
            "wrong": "Anything else.",
            "cited": "Names the orders table."
        },
        "answer": answer
    })
}

async fn post(gateway: &Gateway, path: &str, token: &str, body: &Value) -> (StatusCode, String) {
    let request = Request::post(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, _, text) = send(router(gateway.state.clone()), request).await;
    (status, text)
}

async fn gateway(data: &std::path::Path) -> (Gateway, String) {
    let gateway = Gateway::new(vec![("scout", agent_service(data).await)], None);
    let member = Identity::new("Member", IdentityRole::Member);
    gateway
        .state
        .identity_store
        .as_ref()
        .unwrap()
        .create(&member)
        .unwrap();
    let token = token_for(&member);
    (gateway, token)
}

#[tokio::test]
async fn grading_needs_a_person_not_the_owner_token() {
    let data = tempfile::tempdir().unwrap();
    let (gateway, _) = gateway(data.path()).await;
    let (status, text) = post(
        &gateway,
        "/api/evals/grade",
        GATEWAY_TOKEN,
        &grade_body("42"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
}

#[tokio::test]
async fn a_grade_request_that_names_an_arm_is_rejected() {
    let data = tempfile::tempdir().unwrap();
    let (gateway, member) = gateway(data.path()).await;
    let mut body = grade_body("42");
    body["arm"] = "candidate".into();
    let (status, text) = post(&gateway, "/api/evals/grade", &member, &body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
}

#[tokio::test]
async fn a_blank_answer_comes_back_ungraded() {
    let data = tempfile::tempdir().unwrap();
    let (gateway, member) = gateway(data.path()).await;
    let (status, text) = post(&gateway, "/api/evals/grade", &member, &grade_body(" ")).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let graded: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(graded["status"], "ungraded");
    assert_eq!(graded["cost_usd"], 0.0);
}

#[tokio::test]
async fn grading_on_an_unknown_agent_or_a_pointless_rubric_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let (gateway, member) = gateway(data.path()).await;
    let mut elsewhere = grade_body("42");
    elsewhere["agent_id"] = "ghost".into();
    let (status, text) = post(&gateway, "/api/evals/grade", &member, &elsewhere).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
    let mut pointless = grade_body("42");
    pointless["rubric"]["points"] = json!([]);
    let (status, text) = post(&gateway, "/api/evals/grade", &member, &pointless).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
}

#[tokio::test]
async fn grading_is_refused_on_an_agent_the_person_may_not_talk_to() {
    use workforce_common::settings::{AccessMode, AgentAccessConfig};
    let data = tempfile::tempdir().unwrap();
    let (gateway, member) = gateway(data.path()).await;
    gateway.state.access_gate.upsert(
        "scout".into(),
        AgentAccessConfig {
            mode: AccessMode::Allowlist,
            allow: vec![gateway.admin.id.to_string()],
            ..Default::default()
        },
    );
    let (status, text) = post(&gateway, "/api/evals/grade", &member, &grade_body("42")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
    let body = json!({"message": "hello", "agent_id": "scout"});
    let (status, text) = post(&gateway, "/api/chat", &member, &body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
}

#[tokio::test]
async fn a_grade_request_is_a_rubric_not_a_document() {
    let data = tempfile::tempdir().unwrap();
    let (gateway, member) = gateway(data.path()).await;
    let mut long = grade_body("42");
    long["expected"] = "x".repeat(70_000).into();
    let (status, text) = post(&gateway, "/api/evals/grade", &member, &long).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
}

#[tokio::test]
async fn chat_refuses_a_turn_id_outside_its_alphabet() {
    let data = tempfile::tempdir().unwrap();
    let (gateway, member) = gateway(data.path()).await;
    let body = json!({"message": "hello", "turn_id": "scheduler:job-1"});
    let (status, text) = post(&gateway, "/api/chat", &member, &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
}

#[tokio::test]
async fn counting_know_how_needs_a_person_and_a_config_of_ones_own_an_admin() {
    let data = tempfile::tempdir().unwrap();
    let (gateway, member) = gateway(data.path()).await;
    let live = json!({"agent_id": "scout", "questions": ["a", "b"]});
    let (status, text) = post(&gateway, KNOW_HOW, GATEWAY_TOKEN, &live).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
    let (status, text) = post(&gateway, KNOW_HOW, &member, &live).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let counted: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(counted["matched"].as_array().map(Vec::len), Some(2));

    let mut posed = live.clone();
    posed["config"] =
        "[evals]\ncopy = true\n[know_how]\nenabled = true\nsource_agent = \"other\"\n".into();
    let (status, text) = post(&gateway, KNOW_HOW, &member, &posed).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    let admin = token_for(&gateway.admin);
    let (status, text) = post(&gateway, KNOW_HOW, &admin, &posed).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let counted: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(counted["source_agent"], "other");

    posed["config"] = "[know_how\n".into();
    let (status, text) = post(&gateway, KNOW_HOW, &admin, &posed).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    let elsewhere = json!({"agent_id": "ghost", "questions": []});
    let (status, text) = post(&gateway, KNOW_HOW, &member, &elsewhere).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
}

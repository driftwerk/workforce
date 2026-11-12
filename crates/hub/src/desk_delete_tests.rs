use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{json, Value};
use workforce_common::channels::desk::DeskMark;
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::session::{MessageRole, SessionMessage, DELETED_MESSAGE_NOTE};
use workforce_memory::types::{Memory, MemoryId, MemoryLayer};

use super::*;
use crate::desk_attachments::tests::{agent, call, gateway};
use crate::gateway::GatewayState;

/// A message of `role` saying `text`.
fn say(role: MessageRole, text: &str) -> SessionMessage {
    SessionMessage {
        id: uuid::Uuid::new_v4().to_string(),
        role,
        content: text.to_string(),
        tool_data: None,
        timestamp: chrono::Utc::now(),
        tokens: 5,
        external_id: None,
        attachments: None,
        trust_level: None,
        actor: None,
        desk: None,
    }
}

/// `owner`'s DM with `service`'s agent holding `messages`, kept as a turn
/// keeps them: live and in the archive.
fn dm(service: &AgentService, owner: &str, messages: &[&SessionMessage]) -> String {
    let store = service.sessions.as_ref();
    let mut session = store
        .get_or_create(DESK_CHANNEL, owner, service.agent_id.as_str())
        .unwrap();
    for message in messages {
        store.append_to_archive(&session.id, message).unwrap();
        session.messages.push((*message).clone());
    }
    store.save(&session).unwrap();
    session.id
}

/// A question the person asked under the client's id `client`, and the
/// agent's answer to it.
fn asked(text: &str) -> (SessionMessage, SessionMessage) {
    let client = uuid::Uuid::new_v4();
    let question = SessionMessage {
        external_id: Some(client.to_string()),
        ..say(MessageRole::User, text)
    };
    let answer = SessionMessage {
        desk: Some(DeskMark {
            in_reply_to: Some(client),
            ..DeskMark::default()
        }),
        ..say(MessageRole::Assistant, &format!("About {text}."))
    };
    (question, answer)
}

async fn send(
    state: &Arc<GatewayState>,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(match body {
            Some(body) => axum::body::Body::from(body.to_string()),
            None => axum::body::Body::empty(),
        })
        .unwrap();
    let (status, _, body) = call(state, request).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// What the person reads of their DM: each message's text, and whether it
/// answered a message since deleted.
async fn shown(state: &Arc<GatewayState>, token: &str) -> Vec<(String, bool)> {
    let (status, page) = send(state, "GET", "/api/desk/conversations/scout", token, None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["text"].as_str().unwrap().to_string(),
                m["mark"]["answered_deleted"] == json!(true),
            )
        })
        .collect()
}

/// The person deletes their own message, by its id or the client's: it is
/// gone from what they read, its answer says so, and the agent's context
/// holds only the note. Another person's DM never has it, and the agent's
/// own messages are not theirs to delete.
#[tokio::test(flavor = "multi_thread")]
async fn a_person_deletes_their_own_message_and_no_one_else_s() {
    let dir = tempfile::tempdir().unwrap();
    let (scout, _llm) = agent(dir.path(), "scout", "Seen.", Duration::ZERO).await;
    let (state, [(alice_id, alice), (_, bob)]) = gateway(&scout);
    let (first, first_answer) = asked("pineapples");
    let (second, second_answer) = asked("plums");
    let (third, third_answer) = asked("pears");
    let session = dm(
        &scout,
        &alice_id,
        &[
            &first,
            &first_answer,
            &second,
            &second_answer,
            &third,
            &third_answer,
        ],
    );
    let at = |id: &str| format!("/api/desk/conversations/scout/messages/{id}");

    let (status, _) = send(&state, "DELETE", &at(&first.id), &bob, None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "Bob's DM holds no such message"
    );
    let (status, body) = send(&state, "DELETE", &at(&first_answer.id), &alice, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _) = send(&state, "DELETE", &at("nothing-of-hers"), &alice, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = send(&state, "DELETE", &at(&first.id), &alice, None).await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({"deleted": true, "forgot": []}))
    );
    // By the client's id, with an empty body.
    let client = second.external_id.clone().unwrap();
    let (status, _) = send(&state, "DELETE", &at(&client), &alice, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        shown(&state, &alice).await,
        [
            ("About pineapples.".to_string(), true),
            ("About plums.".to_string(), true),
            ("pears".to_string(), false),
            ("About pears.".to_string(), false),
        ]
    );
    let live = scout.sessions.get(&session).unwrap().unwrap();
    let words: Vec<&str> = live.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        words,
        [
            DELETED_MESSAGE_NOTE,
            "About pineapples.",
            DELETED_MESSAGE_NOTE,
            "About plums.",
            "pears",
            "About pears.",
        ]
    );
    let (status, _) = send(&state, "DELETE", &at(&first.id), &alice, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "deleted once, it is gone");
}

/// Nothing is deleted while the agent is answering in the DM.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_is_not_deleted_while_the_agent_is_answering() {
    let dir = tempfile::tempdir().unwrap();
    let (scout, _llm) = agent(dir.path(), "scout", "Seen.", Duration::ZERO).await;
    let (state, [(alice_id, alice), _]) = gateway(&scout);
    let (question, answer) = asked("pineapples");
    let session = dm(&scout, &alice_id, &[&question, &answer]);
    let (turn, _inbox) = scout
        .live_turns
        .begin(
            &session,
            workforce_agent::tools::approval::SelfServe::Unanswerable,
            None,
            None,
        )
        .await
        .expect("no live turn yet");
    let uri = format!("/api/desk/conversations/scout/messages/{}", question.id);
    let (status, body) = send(&state, "DELETE", &uri, &alice, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"].as_str().unwrap().contains("answering"),
        "{body}"
    );
    drop(turn);
    let (status, _) = send(&state, "DELETE", &uri, &alice, None).await;
    assert_eq!(status, StatusCode::OK);
}

/// What the agent remembered from a message is listed with it, and only
/// what came from it is forgotten with it: not another message's, not
/// another person's, whatever the delete names.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_s_memories_are_listed_and_forgotten_with_it_only_when_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let (scout, _llm) = agent(dir.path(), "scout", "Seen.", Duration::ZERO).await;
    let (state, [(alice_id, alice), (bob_id, _)]) = gateway(&scout);
    let (question, answer) = asked("pineapples");
    let (other, other_answer) = asked("plums");
    let session = dm(
        &scout,
        &alice_id,
        &[&question, &answer, &other, &other_answer],
    );
    let memories = scout.memory_store.clone().expect("the agent remembers");
    let remember = |text: &str, person: &str, from: &[&SessionMessage]| {
        Memory::new(text, MemoryLayer::Episodic, scout.agent_id.clone())
            .with_identity_id(person)
            .with_source_session(&session, from.iter().map(|m| m.id.clone()).collect())
    };
    let from_it = memories
        .remember(remember(
            "Alice likes pineapples",
            &alice_id,
            &[&question, &answer],
        ))
        .await
        .unwrap();
    let from_other = memories
        .remember(remember("Alice likes plums", &alice_id, &[&other]))
        .await
        .unwrap();
    let bobs = memories
        .remember(remember("Bob likes pineapples", &bob_id, &[&question]))
        .await
        .unwrap();

    let uri = format!("/api/desk/conversations/scout/messages/{}", question.id);
    let (status, body) = send(&state, "GET", &format!("{uri}/memories"), &alice, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"memories": [{"id": from_it.0, "text": "Alice likes pineapples"}]})
    );

    let forget = json!({"forget": [from_it.0, from_other.0, bobs.0]});
    let (status, body) = send(&state, "DELETE", &uri, &alice, Some(forget)).await;
    assert_eq!(
        (status, body),
        (
            StatusCode::OK,
            json!({"deleted": true, "forgot": [from_it.0]})
        )
    );
    let kept = |id: &MemoryId| {
        let memories = Arc::clone(&memories);
        let id = id.clone();
        async move { memories.get(&id).await.unwrap().is_some_and(|m| !m.deleted) }
    };
    assert!(!kept(&from_it).await);
    assert!(kept(&from_other).await);
    assert!(kept(&bobs).await);
}

/// A memory came from a message when its provenance names a session of the
/// DM and, among its turn's messages, that one.
#[test]
fn a_memory_comes_from_a_message_its_provenance_names() {
    let chain = vec!["s2".to_string(), "s1".to_string()];
    let memory = |session: Option<&str>, ids: &[&str]| {
        let memory = Memory::new(
            "x",
            MemoryLayer::Episodic,
            workforce_core::types::AgentId::new("scout"),
        );
        match session {
            Some(session) => {
                memory.with_source_session(session, ids.iter().map(|s| s.to_string()).collect())
            }
            None => memory,
        }
    };
    assert!(saved_from(&memory(Some("s1"), &["m1", "m2"]), &chain, "m2"));
    assert!(!saved_from(&memory(Some("s1"), &["m1"]), &chain, "m2"));
    assert!(!saved_from(&memory(Some("s9"), &["m2"]), &chain, "m2"));
    assert!(!saved_from(&memory(None, &[]), &chain, "m2"));
    let forgotten = Memory {
        deleted: true,
        ..memory(Some("s1"), &["m2"])
    };
    assert!(!saved_from(&forgotten, &chain, "m2"));
}

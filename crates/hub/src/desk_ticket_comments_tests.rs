//! A ticket's comments against a fake hub (`desk_board_sync`'s tests'
//! remote MCP and Jira): read from the issue when the ticket is opened,
//! posted to it as the person in their own words, and told apart by who
//! wrote them.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use workforce_postgres::test_support::with_test_pool;

use crate::desk_board_sync::tests::{board, seed, synced_board, LocalHub, Shared};

async fn send(
    local: &LocalHub,
    bearer: &str,
    method: &str,
    uri: &str,
    body: Value,
) -> (StatusCode, Value) {
    let app = super::install(std::sync::Arc::clone(&local.sync));
    let resp = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {bearer}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn said(jira: &Shared, key: &str, id: &str, author: &str, body: &str, app: bool) {
    jira.lock()
        .unwrap()
        .comments
        .entry(key.into())
        .or_default()
        .push((id.into(), author.into(), body.into(), app));
}

fn calls_of(jira: &Shared, tool: &str) -> Vec<Value> {
    jira.lock()
        .unwrap()
        .calls
        .iter()
        .filter(|(t, _)| t == tool)
        .map(|(_, a)| a.clone())
        .collect()
}

/// Opening a ticket reads its issue's comments, a person's and an app's
/// told apart and Desk's own note said as Desk's; a comment from the
/// composer goes to the issue as the person's own words, and comes back
/// as theirs.
#[tokio::test]
async fn comments_are_read_from_the_issue_and_posted_as_the_person() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (local, remote, b) = synced_board(&desk, dir.path()).await;
        seed(&remote.jira, "ACME-1", "Fix the login", &["web"]);
        said(
            &remote.jira,
            "ACME-1",
            "10",
            "Dana Kim",
            "Keep the setup short.",
            false,
        );
        said(
            &remote.jira,
            "ACME-1",
            "11",
            "Release bot",
            "Drafted the section.",
            true,
        );
        said(
            &remote.jira,
            "ACME-1",
            "12",
            "Sam",
            "Desk, on my behalf: Ready for review.",
            false,
        );
        local.sync.pass_board(b.id, true).await.unwrap();
        let t = desk.ticket_by_key("ACME-1").await.unwrap();
        let uri = format!("/api/desk/tickets/{}/comments", t.ticket.id);

        let (status, got) = send(&local, &local.jwt, "GET", &uri, json!(null)).await;
        assert_eq!(status, StatusCode::OK, "{got}");
        let c = got["comments"].as_array().unwrap();
        assert_eq!(c.len(), 3, "{got}");
        assert_eq!(
            c[0]["author"],
            json!({ "kind": "person", "name": "Dana Kim", "on": "Jira" })
        );
        assert_eq!(
            c[0]["body"], "Keep the setup short.",
            "the hub's wrapping is taken off"
        );
        assert_eq!(c[1]["author"]["kind"], "agent");
        assert_eq!(c[2]["author"]["kind"], "desk");
        assert!(c[0].get("remoteId").is_none());
        // Read lately: not again.
        send(&local, &local.jwt, "GET", &uri, json!(null)).await;
        assert_eq!(calls_of(&remote.jira, "jira.list_comments").len(), 1);

        let (status, posted) = send(
            &local,
            &local.jwt,
            "POST",
            &uri,
            json!({ "body": "  On it today.  " }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{posted}");
        assert_eq!(posted["author"]["name"], "You");
        let adds = calls_of(&remote.jira, "jira.add_comment");
        let mine = adds.last().unwrap();
        assert_eq!(mine["issue_key"], "ACME-1");
        assert_eq!(
            mine["body"], "On it today.",
            "the person's own words, no prefix"
        );
        // Read back from the issue, it is still known as theirs.
        desk.read_ticket_comments(
            t.ticket.id,
            &*local
                .sync
                .remote(
                    workforce_desk::board::System::Jira,
                    &workforce_desk::board::Via::Hub {
                        hub: "atlas".into(),
                    },
                    &local.person.id.to_string(),
                    None,
                )
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        let all = desk.ticket_comments(t.ticket.id).await.unwrap();
        let last = all.last().unwrap();
        assert_eq!(last.body, "On it today.");
        assert_eq!(last.author.name, "You");
        assert_eq!(
            last.posted_by.as_ref().map(|p| p.kind),
            Some(workforce_desk::crew::PartyKind::You)
        );

        let (status, _) = send(&local, &local.jwt, "POST", &uri, json!({ "body": "   " })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

/// Only the person who linked the board reads or writes its issues'
/// comments; a ticket of their own keeps its comments here.
#[tokio::test]
async fn only_the_board_s_person_comments_and_a_local_ticket_keeps_its_own() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (local, remote, b) = synced_board(&desk, dir.path()).await;
        seed(&remote.jira, "ACME-1", "Fix the login", &["web"]);
        local.sync.pass_board(b.id, true).await.unwrap();
        let t = desk.ticket_by_key("ACME-1").await.unwrap();
        let uri = format!("/api/desk/tickets/{}/comments", t.ticket.id);
        for bearer in ["gateway-token", "nonsense"] {
            let (status, _) = send(&local, bearer, "POST", &uri, json!({ "body": "hi" })).await;
            assert!(matches!(
                status,
                StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
            ));
        }
        assert!(calls_of(&remote.jira, "jira.add_comment").is_empty());

        let mine = board(&desk, "Mine").await;
        let lt = desk
            .create_ticket(
                workforce_desk::NewTicket {
                    board: mine.id,
                    title: "Tidy the docs".into(),
                    body: String::new(),
                    state: None,
                    needs: vec![],
                },
                &workforce_desk::crew::By::you(),
            )
            .await
            .unwrap();
        let uri = format!("/api/desk/tickets/{}/comments", lt.ticket.id);
        let (status, _) = send(
            &local,
            &local.jwt,
            "POST",
            &uri,
            json!({ "body": "A note to self" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (_, got) = send(&local, &local.jwt, "GET", &uri, json!(null)).await;
        assert_eq!(got["comments"][0]["body"], "A note to self");
        assert_eq!(
            got["comments"][0]["onlyHere"], false,
            "a local ticket's are all here"
        );
    })
    .await;
}

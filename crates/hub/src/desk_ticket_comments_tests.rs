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
/// told apart, and words there that say they are Desk's still a person's,
/// shown as written; a comment from the composer goes to the issue as the
/// person's own words, and comes back as theirs.
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
        assert_eq!(c[2]["author"]["kind"], "person");
        assert_eq!(c[2]["body"], "Desk, on my behalf: Ready for review.");
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
        assert!(
            got.get("history").is_none(),
            "a local ticket has none: {got}"
        );
        assert_eq!(
            got["comments"][0]["onlyHere"], false,
            "a local ticket's are all here"
        );
    })
    .await;
}

/// A comment goes out in the person's own words only from the person at
/// their own Desk: the Desk app on their computer may post; a phone paired
/// to them, and a token made for an agent or a session (a remote MCP one),
/// may not, and nothing is posted or kept.
#[tokio::test]
async fn only_the_person_at_their_desk_posts_in_their_own_words() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        use crate::remote_mcp::testing::{issue, pair_device, pair_device_as, TokenShape};
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (local, remote, b) = synced_board(&desk, dir.path()).await;
        seed(&remote.jira, "ACME-1", "Fix the login", &["web"]);
        local.sync.pass_board(b.id, true).await.unwrap();
        let t = desk.ticket_by_key("ACME-1").await.unwrap();
        let uri = format!("/api/desk/tickets/{}/comments", t.ticket.id);
        let mut sam = local.person.clone();
        let (_, phone) = pair_device_as(
            &local.fx,
            &local.devices,
            &mut sam,
            "Sam's phone",
            "ios_device",
        );
        let (_, mac) = pair_device(&local.fx, &local.devices, &mut sam, "Sam's Mac");
        let agent = issue(&local.fx, &sam, TokenShape::default());

        for (bearer, what) in [(&phone, "a phone"), (&agent, "an agent's token")] {
            let (status, got) =
                send(&local, bearer, "POST", &uri, json!({ "body": "Ship it" })).await;
            assert!(
                matches!(status, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED),
                "{what}: {status} {got}"
            );
        }
        assert!(calls_of(&remote.jira, "jira.add_comment").is_empty());
        assert!(desk.ticket_comments(t.ticket.id).await.unwrap().is_empty());
        // A phone still reads them.
        let (status, _) = send(&local, &phone, "GET", &uri, json!(null)).await;
        assert_eq!(status, StatusCode::OK);

        let (status, got) = send(&local, &mac, "POST", &uri, json!({ "body": "Ship it" })).await;
        assert_eq!(status, StatusCode::CREATED, "{got}");
        assert_eq!(calls_of(&remote.jira, "jira.add_comment").len(), 1);
    })
    .await;
}

/// Only the person posts to the issue unprefixed: Desk refuses a session's
/// comment there, whoever asks it to.
#[tokio::test]
async fn no_one_but_the_person_posts_to_the_issue() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (local, remote, b) = synced_board(&desk, dir.path()).await;
        seed(&remote.jira, "ACME-1", "Fix the login", &["web"]);
        local.sync.pass_board(b.id, true).await.unwrap();
        let t = desk.ticket_by_key("ACME-1").await.unwrap();
        let via = local
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
            .unwrap();
        let link = t.ticket.external.as_ref().unwrap().link;
        let session = workforce_desk::crew::By {
            party: workforce_desk::crew::Party {
                kind: workforce_desk::crew::PartyKind::Session,
                id: uuid::Uuid::new_v4().to_string(),
                name: "acme-web".into(),
                engine: None,
                origin: None,
            },
            ..workforce_desk::crew::By::you()
        };
        let refused = desk
            .post_ticket_comment(
                t.ticket.id,
                "Ship it",
                &session,
                Some(workforce_desk::PostVia {
                    remote: &*via,
                    link,
                }),
            )
            .await;
        assert!(
            matches!(refused, Err(workforce_desk::CommentPost::Desk(_))),
            "{refused:?}"
        );
        assert!(calls_of(&remote.jira, "jira.add_comment").is_empty());
        assert!(desk.ticket_comments(t.ticket.id).await.unwrap().is_empty());
    })
    .await;
}

/// A ticket kept on its board after it stopped syncing takes no comment:
/// 409, nothing posted or kept.
#[tokio::test]
async fn a_kept_ticket_takes_no_comment() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (local, remote, b) = synced_board(&desk, dir.path()).await;
        seed(&remote.jira, "ACME-1", "Fix the login", &["web"]);
        local.sync.pass_board(b.id, true).await.unwrap();
        crate::desk_board_sync::tests::touch(&remote.jira, "ACME-1", |i| i.labels.clear());
        // A sweep is due again: it finds the issue left the filter.
        desk.change_sync(b.id, |s| s.swept_at = None).await.unwrap();
        local.sync.pass_board(b.id, true).await.unwrap();
        let t = desk.ticket_by_key("ACME-1").await.unwrap();
        desk.left_filter(t.ticket.id, true, &workforce_desk::crew::By::you())
            .await
            .unwrap();
        let t = desk.ticket_by_key("ACME-1").await.unwrap();
        assert!(t.ticket.external.as_ref().unwrap().kept);
        let uri = format!("/api/desk/tickets/{}/comments", t.ticket.id);
        let (status, got) = send(
            &local,
            &local.jwt,
            "POST",
            &uri,
            json!({ "body": "Ship it" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{got}");
        assert!(calls_of(&remote.jira, "jira.add_comment").is_empty());
        assert!(desk.ticket_comments(t.ticket.id).await.unwrap().is_empty());
    })
    .await;
}

/// An answer that lists no comments fails the read and deletes nothing:
/// the person's own comment and the issue's stay as they were.
#[tokio::test]
async fn a_malformed_read_keeps_every_comment() {
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
            "Keep it short.",
            false,
        );
        local.sync.pass_board(b.id, true).await.unwrap();
        let t = desk.ticket_by_key("ACME-1").await.unwrap();
        let uri = format!("/api/desk/tickets/{}/comments", t.ticket.id);
        let (status, _) = send(&local, &local.jwt, "POST", &uri, json!({ "body": "On it" })).await;
        assert_eq!(status, StatusCode::CREATED);
        let via = local
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
            .unwrap();
        desk.read_ticket_comments(t.ticket.id, &*via).await.unwrap();
        assert_eq!(desk.ticket_comments(t.ticket.id).await.unwrap().len(), 2);
        for answer in [
            json!({ "comments": "none" }),
            json!({ "issue_key": "ACME-1" }),
        ] {
            remote.jira.lock().unwrap().comments_answer = Some(answer);
            assert!(desk.read_ticket_comments(t.ticket.id, &*via).await.is_err());
            assert_eq!(desk.ticket_comments(t.ticket.id).await.unwrap().len(), 2);
        }
        // A page of fewer than the issue has: kept in step, nothing deleted.
        remote.jira.lock().unwrap().comments_answer = Some(json!({ "total": 5, "comments": [] }));
        desk.read_ticket_comments(t.ticket.id, &*via).await.unwrap();
        let all = desk.ticket_comments(t.ticket.id).await.unwrap();
        assert_eq!(all.len(), 2);
        // A whole read listing none, asked for after the person posted: the
        // issue's go, and so does theirs, deleted there too.
        remote.jira.lock().unwrap().comments_answer = Some(json!({ "total": 0, "comments": [] }));
        desk.read_ticket_comments(t.ticket.id, &*via).await.unwrap();
        assert!(desk.ticket_comments(t.ticket.id).await.unwrap().is_empty());
    })
    .await;
}

fn fenced(s: &str) -> String {
    workforce_agent::security::external_content::wrap_external_content(
        s,
        "jira.get_issue.changelog",
    )
    .wrapped_content
}

/// Opening a ticket reads its issue's history beside its comments, in the
/// shape Desk reads (`history`, camelCase, unwrapped, oldest first); and a
/// Jira issue's comments past a page are all read, a page at a time.
#[tokio::test]
async fn history_comes_beside_the_comments_and_comments_past_a_page_are_read() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (local, remote, b) = synced_board(&desk, dir.path()).await;
        seed(&remote.jira, "ACME-1", "Fix the login", &["web"]);
        for n in 0..130 {
            said(
                &remote.jira,
                "ACME-1",
                &format!("{}", 1000 + n),
                "Dana Kim",
                &format!("Note {n}"),
                false,
            );
        }
        remote.jira.lock().unwrap().history.insert(
            "ACME-1".into(),
            json!({
                "created": "2026-11-01T09:00:00.000+0000",
                "reporter": { "display_name": "Dana Kim", "account_id": "acc-dana" },
                "changelog": { "total": 2, "histories": [
                    { "id": "2", "created": "2026-11-03T09:00:00.000+0000", "author": "Sam",
                      "author_account_id": crate::desk_board_sync::tests::SAM_ACCOUNT,
                      "author_account_type": "atlassian",
                      "items": [{ "field": "assignee", "field_id": "assignee", "from": null, "to": fenced("Sam") }] },
                    { "id": "1", "created": "2026-11-02T09:00:00.000+0000", "author": "Dana Kim",
                      "author_account_id": "acc-dana", "author_account_type": "atlassian",
                      "items": [
                        { "field": "status", "field_id": "status", "from": fenced("To Do"), "to": fenced("In Progress") },
                        { "field": "Rank", "field_id": "customfield_10019", "from": null, "to": fenced("Ranked higher") },
                      ] },
                ] }
            }),
        );
        local.sync.pass_board(b.id, true).await.unwrap();
        let t = desk.ticket_by_key("ACME-1").await.unwrap();
        let uri = format!("/api/desk/tickets/{}/comments", t.ticket.id);

        let (status, got) = send(&local, &local.jwt, "GET", &uri, json!(null)).await;
        assert_eq!(status, StatusCode::OK, "{got}");
        assert_eq!(
            got["history"],
            json!([
                { "at": "2026-11-01T09:00:00Z", "actor": "Dana Kim", "by": "remote", "kind": "created" },
                { "at": "2026-11-02T09:00:00Z", "actor": "Dana Kim", "by": "remote", "kind": "state",
                  "from": "To Do", "to": "In Progress" },
                { "at": "2026-11-03T09:00:00Z", "actor": "Sam", "by": "remote", "kind": "assignee", "to": "Sam" },
            ]),
            "on the person's own key, their account's changes are theirs"
        );
        let asked = calls_of(&remote.jira, "jira.get_issue");
        assert!(
            asked.iter().any(|a| a["changelog"] == json!(true)),
            "{asked:?}"
        );
        let c = got["comments"].as_array().unwrap();
        assert_eq!(c.len(), 130, "past the first page");
        assert_eq!(c[129]["body"], "Note 129");
        assert!(c.iter().all(|c| c.get("parent").is_none()), "Jira's are flat");
        let pages: Vec<Value> = calls_of(&remote.jira, "jira.list_comments")
            .iter()
            .map(|a| a["start_at"].clone())
            .collect();
        assert_eq!(pages, [json!(0), json!(100)]);

        // Read lately: neither is read again, and what was read is kept.
        let (_, again) = send(&local, &local.jwt, "GET", &uri, json!(null)).await;
        assert_eq!(again["history"], got["history"]);
        assert_eq!(calls_of(&remote.jira, "jira.list_comments").len(), 2);
    })
    .await;
}

/// An issue whose history can't be read still shows its comments, with no
/// problem said, and no `history`.
#[tokio::test]
async fn a_history_that_can_t_be_read_never_stops_the_comments() {
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
            "Keep it short.",
            false,
        );
        local.sync.pass_board(b.id, true).await.unwrap();
        let t = desk.ticket_by_key("ACME-1").await.unwrap();
        // An older hub: its issue comes back without a changelog.
        let uri = format!("/api/desk/tickets/{}/comments", t.ticket.id);
        let (status, got) = send(&local, &local.jwt, "GET", &uri, json!(null)).await;
        assert_eq!(status, StatusCode::OK, "{got}");
        assert_eq!(got["comments"].as_array().unwrap().len(), 1);
        assert!(got["problem"].is_null(), "{got}");
        assert!(got.get("history").is_none(), "{got}");

        // The history call itself refused: the comments are read all the
        // same, and nothing is said of it.
        remote.jira.lock().unwrap().refuse = Some("jira.get_issue");
        let via = local
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
            .unwrap();
        assert!(desk.read_remote_history(t.ticket.id, &*via).await.is_err());
        said(&remote.jira, "ACME-1", "11", "Sam", "On it.", false);
        let read = desk.read_ticket_comments(t.ticket.id, &*via).await.unwrap();
        assert_eq!(read.len(), 2);
        assert!(desk.remote_history(t.ticket.id).await.unwrap().is_none());
    })
    .await;
}

/// A comment posted but not kept here is answered as posted, with no id:
/// one would name nothing Desk could find again.
#[test]
fn a_comment_not_kept_is_answered_with_no_id() {
    use workforce_desk::ticket_comments::{Author, AuthorKind, Comment};
    let c = Comment {
        id: uuid::Uuid::nil(),
        remote_id: Some("10042".into()),
        author: Author {
            kind: AuthorKind::Person,
            name: "You".into(),
            on: "Desk".into(),
        },
        posted_by: None,
        body: "On it".into(),
        flagged: false,
        at: chrono::Utc::now(),
        edited_at: None,
        only_here: false,
        parent: None,
    };
    let answer = super::not_kept(&c);
    assert!(answer["id"].is_null(), "{answer}");
    assert_eq!(answer["kept"], false);
    assert_eq!(answer["body"], "On it");
    assert!(answer.get("remoteId").is_none(), "{answer}");
    assert!(answer["message"].as_str().unwrap().starts_with("Posted."));
}

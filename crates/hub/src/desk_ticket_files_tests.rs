//! A ticket's files against a fake hub (`desk_board_sync_tests`' remote
//! MCP and Jira): an issue's attachments come in with the pull, their bytes
//! are read through the board's sync as the person who linked it, chunk by
//! chunk, and served only as what they are.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use workforce_desk::ticket_files::FileSource;
use workforce_postgres::test_support::with_test_pool;

use crate::desk_board_sync::tests::{board, seed, synced_board, LocalHub, Shared};

/// A PNG's first bytes, then enough to take three chunks.
fn png(len: usize) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
    b.extend((0..len).map(|i| (i % 251) as u8));
    b
}

fn attach(jira: &Shared, key: &str, id: &str, name: &str, mime: &str, bytes: Vec<u8>) {
    jira.lock()
        .unwrap()
        .files
        .entry(key.into())
        .or_default()
        .push((id.into(), name.into(), mime.into(), bytes));
}

struct Answer {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }
}

async fn send(local: &LocalHub, bearer: &str, method: &str, uri: &str, body: Vec<u8>) -> Answer {
    let app = super::install(std::sync::Arc::clone(&local.sync));
    let resp = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), 64 << 20)
        .await
        .unwrap()
        .to_vec();
    Answer {
        status,
        headers,
        body,
    }
}

/// The pull lists an issue's attachments on its ticket; a file's bytes are
/// read chunk by chunk through the board's own sync and kept, so a second
/// read calls nothing; what's served is what the bytes are.
#[tokio::test]
async fn an_issue_s_attachments_come_in_and_are_read_through_its_sync() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (local, remote, b) = synced_board(&desk, dir.path()).await;
        seed(&remote.jira, "ACME-1", "Fix the login", &["web"]);
        let shot = png(1_200_000);
        attach(
            &remote.jira,
            "ACME-1",
            "10001",
            "login.png",
            "image/png",
            shot.clone(),
        );
        attach(
            &remote.jira,
            "ACME-1",
            "10002",
            "<script>evil</script>.svg",
            "image/svg+xml",
            b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>".to_vec(),
        );
        local.sync.pass_board(b.id, true).await.unwrap();
        let t = desk.ticket_by_key("ACME-1").await.unwrap();
        assert_eq!(t.files.len(), 2, "{:?}", t.files);
        let png_file = &t.files[0];
        assert_eq!(png_file.source, FileSource::Jira);
        assert_eq!(
            png_file.name, "login.png",
            "the hub's wrapping is taken off"
        );
        assert_eq!(png_file.kind(), "image");
        let page = serde_json::to_value(png_file).unwrap();
        assert!(
            page.get("remoteId").is_none(),
            "the issue's id never reaches the page"
        );

        let uri = format!("/api/desk/tickets/{}/files/{}", t.ticket.id, png_file.id);
        let a = send(&local, &local.jwt, "GET", &uri, vec![]).await;
        assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
        assert_eq!(a.body, shot);
        assert_eq!(a.header("content-type"), "image/png");
        assert_eq!(a.header("x-content-type-options"), "nosniff");
        assert_eq!(
            a.header("content-security-policy"),
            "sandbox; default-src 'none'"
        );
        assert!(a.header("content-disposition").starts_with("inline;"));
        let reads = |j: &Shared| {
            j.lock()
                .unwrap()
                .calls
                .iter()
                .filter(|(t, _)| t == "jira.attachment_read")
                .count()
        };
        assert_eq!(reads(&remote.jira), 3, "read in chunks of 512 KiB");
        // Kept: the next read calls nothing.
        let again = send(&local, &local.jwt, "GET", &uri, vec![]).await;
        assert_eq!(again.body, shot);
        assert_eq!(reads(&remote.jira), 3);

        // An SVG is never a picture: unknown bytes, a download, its name
        // made safe for a header.
        let svg = &t.files[1];
        let a = send(
            &local,
            &local.jwt,
            "GET",
            &format!("/api/desk/tickets/{}/files/{}", t.ticket.id, svg.id),
            vec![],
        )
        .await;
        assert_eq!(a.status, StatusCode::OK);
        assert_eq!(a.header("content-type"), "application/octet-stream");
        let disposition = a.header("content-disposition");
        assert!(disposition.starts_with("attachment;"), "{disposition}");
        assert!(!disposition.contains('<'), "{disposition}");

        // An attachment taken off the issue goes from the ticket.
        remote
            .jira
            .lock()
            .unwrap()
            .files
            .get_mut("ACME-1")
            .unwrap()
            .remove(1);
        crate::desk_board_sync::tests::touch(&remote.jira, "ACME-1", |_| {});
        local.sync.pass_board(b.id, true).await.unwrap();
        let t = desk.ticket_by_key("ACME-1").await.unwrap();
        assert_eq!(t.files.len(), 1);
    })
    .await;
}

/// Only the person who linked the board reads its files, and only a file
/// of the ticket asked about; the hub's own token is no person.
#[tokio::test]
async fn a_file_is_read_only_by_its_board_s_person_and_by_its_own_ticket() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (local, remote, b) = synced_board(&desk, dir.path()).await;
        seed(&remote.jira, "ACME-1", "Fix the login", &["web"]);
        seed(&remote.jira, "ACME-2", "Fix the logout", &["web"]);
        attach(
            &remote.jira,
            "ACME-1",
            "10001",
            "a.png",
            "image/png",
            png(10),
        );
        local.sync.pass_board(b.id, true).await.unwrap();
        let one = desk.ticket_by_key("ACME-1").await.unwrap();
        let two = desk.ticket_by_key("ACME-2").await.unwrap();
        let file = one.files[0].id;
        let a = send(
            &local,
            &local.jwt,
            "GET",
            &format!("/api/desk/tickets/{}/files/{file}", two.ticket.id),
            vec![],
        )
        .await;
        assert_eq!(a.status, StatusCode::NOT_FOUND);
        let a = send(
            &local,
            "gateway-token",
            "GET",
            &format!("/api/desk/tickets/{}/files/{file}", one.ticket.id),
            vec![],
        )
        .await;
        assert_eq!(a.status, StatusCode::FORBIDDEN);
        // Another person who holds this hub too, but didn't link the board.
        let state = &local.sync.state;
        let alex = workforce_identity::types::Identity::new(
            "Alex",
            workforce_identity::types::IdentityRole::SystemAdmin,
        );
        state
            .identity_store
            .as_ref()
            .unwrap()
            .create(&alex)
            .unwrap();
        let alex_jwt = crate::jwt::generate_token_with_identity(
            state.jwt_config.as_ref().unwrap(),
            &alex.display_name,
            alex.role,
            Some(alex.id.to_string()),
        )
        .unwrap();
        let a = send(
            &local,
            &alex_jwt,
            "GET",
            &format!("/api/desk/tickets/{}/files/{file}", one.ticket.id),
            vec![],
        )
        .await;
        assert_eq!(a.status, StatusCode::FORBIDDEN);
        assert_eq!(
            a.json()["error"],
            "Only the person who linked this board reads and writes its issues.",
            "refused by the board's person, not by the hub"
        );
        let a = send(
            &local,
            "nonsense",
            "GET",
            &format!("/api/desk/tickets/{}/files/{file}", one.ticket.id),
            vec![],
        )
        .await;
        assert_eq!(a.status, StatusCode::UNAUTHORIZED);
        assert!(
            remote
                .jira
                .lock()
                .unwrap()
                .calls
                .iter()
                .all(|(t, _)| t != "jira.attachment_read"),
            "nothing was read for a refused ask"
        );
    })
    .await;
}

/// A file dropped on a ticket of the person's own is kept here and served
/// as what its bytes are; a synced ticket takes none; a ticket holds so many.
#[tokio::test]
async fn a_file_dropped_on_a_local_ticket_is_kept_and_served() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (local, remote, synced) = synced_board(&desk, dir.path()).await;
        let mine = board(&desk, "Mine").await;
        let t = desk
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
        let shot = png(2_000);
        let a = send(
            &local,
            &local.jwt,
            "POST",
            &format!("/api/desk/tickets/{}/files?name=shot.png", t.ticket.id),
            shot.clone(),
        )
        .await;
        assert_eq!(a.status, StatusCode::CREATED, "{:?}", a.json());
        let id = a.json()["id"].as_str().unwrap().to_string();
        assert_eq!(
            a.json()["mediaType"],
            "image/png",
            "sniffed, not taken from the name"
        );
        let got = send(
            &local,
            &local.jwt,
            "GET",
            &format!("/api/desk/tickets/{}/files/{id}", t.ticket.id),
            vec![],
        )
        .await;
        assert_eq!(got.body, shot);
        // An HTML file named like a picture is still a download.
        let a = send(
            &local,
            &local.jwt,
            "POST",
            &format!("/api/desk/tickets/{}/files?name=look.png", t.ticket.id),
            b"<html><script>alert(1)</script></html>".to_vec(),
        )
        .await;
        let html = a.json()["id"].as_str().unwrap().to_string();
        let got = send(
            &local,
            &local.jwt,
            "GET",
            &format!("/api/desk/tickets/{}/files/{html}", t.ticket.id),
            vec![],
        )
        .await;
        assert_eq!(got.header("content-type"), "application/octet-stream");
        // Taken off again.
        let a = send(
            &local,
            &local.jwt,
            "DELETE",
            &format!("/api/desk/tickets/{}/files/{id}", t.ticket.id),
            vec![],
        )
        .await;
        assert_eq!(a.status, StatusCode::NO_CONTENT);
        assert_eq!(desk.ticket(t.ticket.id).await.unwrap().files.len(), 1);

        // A synced ticket's files are its issue's.
        seed(&remote.jira, "ACME-1", "Fix the login", &["web"]);
        local.sync.pass_board(synced.id, true).await.unwrap();
        let s = desk.ticket_by_key("ACME-1").await.unwrap();
        let a = send(
            &local,
            &local.jwt,
            "POST",
            &format!("/api/desk/tickets/{}/files?name=x.png", s.ticket.id),
            png(10),
        )
        .await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST);
    })
    .await;
}

/// Opening a synced ticket reads its issue whole for its files, once in a
/// while: a second open soon after calls nothing.
#[tokio::test]
async fn opening_a_ticket_reads_its_issue_once_in_a_while() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (local, remote, b) = synced_board(&desk, dir.path()).await;
        seed(&remote.jira, "ACME-1", "Fix the login", &["web"]);
        local.sync.pass_board(b.id, true).await.unwrap();
        let t = desk.ticket_by_key("ACME-1").await.unwrap();
        assert!(t.files.is_empty());
        // Attached since, with no change the pull sees.
        attach(
            &remote.jira,
            "ACME-1",
            "10009",
            "spec.pdf",
            "application/pdf",
            b"%PDF-1.7 x".to_vec(),
        );
        let gets = |j: &Shared| {
            j.lock()
                .unwrap()
                .calls
                .iter()
                .filter(|(t, _)| t == "jira.get_issue")
                .count()
        };
        let before = gets(&remote.jira);
        let uri = format!("/api/desk/tickets/{}/read", t.ticket.id);
        let a = send(&local, &local.jwt, "POST", &uri, vec![]).await;
        assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
        assert_eq!(a.json()["files"][0]["name"], "spec.pdf");
        assert_eq!(a.json()["files"][0]["mediaType"], "application/pdf");
        assert_eq!(gets(&remote.jira), before + 1);
        let a = send(&local, &local.jwt, "POST", &uri, vec![]).await;
        assert_eq!(a.status, StatusCode::OK);
        assert_eq!(gets(&remote.jira), before + 1, "read lately: not again");
        let a = send(
            &local,
            &local.jwt,
            "POST",
            &format!("{uri}?force=1"),
            vec![],
        )
        .await;
        assert_eq!(a.status, StatusCode::OK);
        assert_eq!(gets(&remote.jira), before + 2);
    })
    .await;
}

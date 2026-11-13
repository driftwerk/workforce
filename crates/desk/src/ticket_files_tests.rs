//! Reading an issue's files out of its answers, serving bytes as what they
//! are, reading a remote file chunk by chunk from a remote that may lie,
//! and keeping the list and the cache in Postgres.

use std::sync::Mutex;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};
use workforce_postgres::test_support::with_test_pool;

use super::*;
use crate::crew::PartyKind;
use crate::store::build_pool;

fn someone() -> Party {
    Party {
        kind: PartyKind::Desk,
        id: "jira:ACME".into(),
        name: "Jira".into(),
        engine: None,
        origin: None,
    }
}

#[test]
fn a_jira_answer_lists_its_attachments_by_id_and_only_digits() {
    let issue = json!({ "attachments": [
        { "id": "10001", "filename": "<<<EXTERNAL_UNTRUSTED_CONTENT_ab>>>\n[External content from jira]\nshot.png\n<<</EXTERNAL_UNTRUSTED_CONTENT_ab>>>",
          "mime_type": "IMAGE/PNG", "size": 240_000, "thumbnail": true },
        { "id": "../../secure", "filename": "x", "mime_type": "image/png" },
        { "id": 10003, "filename": null, "mime_type": null },
    ]});
    let files = jira_files(&issue).unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].name, "shot.png");
    assert_eq!(files[0].media_type, "image/png");
    assert!(files[0].thumb);
    assert_eq!(files[1].remote_id, "10003");
    assert_eq!(files[1].name, "attachment-10003");
    assert!(
        jira_files(&json!({ "key": "ACME-1" })).is_none(),
        "an older hub's answer says nothing"
    );
}

#[test]
fn a_linear_description_s_uploads_and_its_links_are_its_files() {
    let body = "Steps:\n![the login page](https://uploads.linear.app/a1/b2/login.png)\n\
                See [spec.pdf](https://uploads.linear.app/a1/c3/spec.pdf) and \
                ![x](https://evil.example/uploads.linear.app/pixel.png) and \
                ![again](https://uploads.linear.app/a1/b2/login.png)";
    let issue = json!({ "attachments": { "nodes": [
        { "id": "att-1", "title": "README layout", "url": "https://www.figma.com/file/abc", "sourceType": "figma" },
        { "id": "att-2", "title": "unsafe", "url": "javascript:alert(1)" },
        { "id": "att-3", "title": "Plain", "url": "http://example.com/x" },
    ]}});
    let files = linear_files(&issue, body);
    let names: Vec<_> = files.iter().map(|f| (f.source, f.name.as_str())).collect();
    assert_eq!(
        names,
        [
            (FileSource::LinearUpload, "the login page"),
            (FileSource::LinearUpload, "spec.pdf"),
            (FileSource::LinearLink, "README layout"),
        ]
    );
    assert_eq!(files[0].media_type, "image/png");
    assert_eq!(files[1].media_type, "application/pdf");
    assert_eq!(
        files[2].url.as_deref(),
        Some("https://www.figma.com/file/abc")
    );
}

#[test]
fn bytes_are_what_their_first_bytes_say() {
    assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
    assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
    assert_eq!(sniff(b"GIF89a.."), Some("image/gif"));
    assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
    assert_eq!(sniff(b"%PDF-1.7"), Some("application/pdf"));
    for unknown in [
        &b"<svg xmlns='http://www.w3.org/2000/svg'/>"[..],
        b"<!doctype html><script>",
        b"",
    ] {
        assert_eq!(served_as(unknown), ("application/octet-stream", false));
    }
    assert_eq!(served_as(b"%PDF-1.4"), ("application/pdf", true));
}

#[test]
fn a_filename_is_safe_in_a_header() {
    assert_eq!(safe_filename("shot.png"), "shot.png");
    assert_eq!(safe_filename("a\"; evil=\"b\r\n.png"), "a__ evil__b__.png");
    assert_eq!(safe_filename("../../.env"), "_.._.env");
    assert_eq!(safe_filename(".."), "file");
}

/// A remote that answers chunks of `bytes`, lying as told.
struct Chunks {
    bytes: Vec<u8>,
    lie: Option<&'static str>,
    calls: Mutex<Vec<Value>>,
}

#[async_trait]
impl Remote for Chunks {
    async fn tools(&self) -> std::result::Result<Vec<String>, RemoteError> {
        Ok(vec![])
    }
    async fn call(&self, tool: &str, args: Value) -> std::result::Result<Value, RemoteError> {
        assert_eq!(tool, "jira.attachment_read");
        self.calls.lock().unwrap().push(args.clone());
        let offset = args["offset"].as_u64().unwrap() as usize;
        let len = args["length"].as_u64().unwrap() as usize;
        let end = (offset + len).min(self.bytes.len());
        let mut v = json!({
            "kind": "file_bytes", "media_type": "image/png", "size": self.bytes.len(),
            "offset": offset, "length": end - offset,
            "data": base64::engine::general_purpose::STANDARD.encode(&self.bytes[offset..end]),
        });
        match self.lie {
            Some("offset") => v["offset"] = json!(offset + 1),
            Some("huge") => v["size"] = json!(FILE_MAX + 1),
            Some("growing") => v["size"] = json!(self.bytes.len() + offset),
            Some("kind") => v["kind"] = json!("rows"),
            Some("empty") => {
                v["data"] = json!("");
                v["length"] = json!(0);
            }
            _ => {}
        }
        Ok(v)
    }
}

fn jira_file(size: Option<u64>) -> TicketFile {
    TicketFile {
        id: Uuid::new_v4(),
        source: FileSource::Jira,
        remote_id: Some("10001".into()),
        name: "shot.png".into(),
        media_type: "image/png".into(),
        size,
        url: None,
        thumb: false,
        added_by: someone(),
        added_at: Utc::now(),
    }
}

#[tokio::test]
async fn a_remote_file_is_read_chunk_by_chunk_and_a_lie_stops_it() {
    let bytes: Vec<u8> = (0..1_100_000u32).map(|i| (i % 253) as u8).collect();
    let honest = Chunks {
        bytes: bytes.clone(),
        lie: None,
        calls: Mutex::default(),
    };
    let got = read_remote(&honest, System::Jira, "ACME-1", &jira_file(None), false)
        .await
        .unwrap();
    assert_eq!(got, bytes);
    {
        let calls = honest.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0]["attachment_id"], "10001");
        assert_eq!(calls[0]["issue_key"], "ACME-1");
    }
    for lie in ["offset", "huge", "growing", "kind", "empty"] {
        let liar = Chunks {
            bytes: bytes.clone(),
            lie: Some(lie),
            calls: Mutex::default(),
        };
        let r = read_remote(&liar, System::Jira, "ACME-1", &jira_file(None), false).await;
        assert!(r.is_err(), "{lie}");
        assert!(liar.calls.lock().unwrap().len() <= 2, "{lie} stopped early");
    }
    // Said to be too big: never read.
    let r = read_remote(
        &honest,
        System::Jira,
        "ACME-1",
        &jira_file(Some(FILE_MAX + 1)),
        false,
    )
    .await;
    assert!(matches!(r, Err(FileRead::TooBig)));
    // A link has no bytes.
    let mut link = jira_file(None);
    link.source = FileSource::LinearLink;
    assert!(matches!(
        read_remote(&honest, System::Linear, "ATL-1", &link, false).await,
        Err(FileRead::Link)
    ));
}

async fn a_ticket(store: &DeskStore) -> Uuid {
    let client = store.pool().get().await.unwrap();
    let board = Uuid::new_v4();
    let ticket = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO boards (id, name, prefix, created_at, updated_at) VALUES ($1, 'B', $2, now(), now())",
            &[&board, &format!("B{}", &board.simple().to_string()[..4].to_ascii_uppercase().replace(|c: char| c.is_ascii_digit(), "X"))],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO tickets (id, board, number, title, state, rank, created_by, state_at, created_at, updated_at) \
             VALUES ($1, $2, 1, 'T', 'todo', 1, '{\"kind\":\"you\",\"id\":\"you\",\"name\":\"You\"}', now(), now(), now())",
            &[&ticket, &board],
        )
        .await
        .unwrap();
    ticket
}

fn remote(id: &str, name: &str) -> RemoteFile {
    RemoteFile {
        source: FileSource::Jira,
        remote_id: id.into(),
        name: name.into(),
        media_type: "image/png".into(),
        size: Some(10),
        url: None,
        thumb: false,
    }
}

#[tokio::test]
async fn an_issue_s_files_are_kept_in_step_and_the_cache_lets_go_of_the_oldest() {
    with_test_pool(build_pool, |pool| async move {
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool);
        let t = a_ticket(&store).await;
        let by = someone();
        assert!(store
            .put_remote_files(t, &[remote("1", "a.png"), remote("2", "b.png")], &by)
            .await
            .unwrap());
        assert!(
            !store
                .put_remote_files(t, &[remote("1", "a.png"), remote("2", "b.png")], &by)
                .await
                .unwrap(),
            "the same list changes nothing"
        );
        let upload = store
            .add_upload(
                t,
                "mine.png",
                "image/png",
                b"\x89PNG\r\n\x1a\n",
                &Party::you(),
            )
            .await
            .unwrap();
        let Uploaded::Added(upload) = upload else {
            panic!("{upload:?}")
        };
        let files = &store.files_of(&[t]).await.unwrap()[&t];
        let (a, b) = (files[0].id, files[1].id);
        store
            .keep_file_bytes_within(a, false, &[1; 600], 1000)
            .await
            .unwrap();
        store
            .keep_file_bytes_within(b, false, &[2; 600], 1000)
            .await
            .unwrap();
        assert_eq!(
            store.file_bytes(a, false).await.unwrap(),
            None,
            "the older went"
        );
        assert_eq!(
            store.file_bytes(b, false).await.unwrap(),
            Some(vec![2; 600])
        );
        // Renamed there: its kept bytes go with the old name.
        store
            .put_remote_files(t, &[remote("2", "b2.png")], &by)
            .await
            .unwrap();
        let files = &store.files_of(&[t]).await.unwrap()[&t];
        assert_eq!(files.len(), 2, "a gone, b renamed, the upload stays");
        assert_eq!(files[0].name, "b2.png");
        assert_eq!(store.file_bytes(b, false).await.unwrap(), None);
        assert_eq!(files[1].id, upload.id);
        assert_eq!(
            store.file_bytes(upload.id, false).await.unwrap().as_deref(),
            Some(&b"\x89PNG\r\n\x1a\n"[..]),
            "an upload's bytes are never let go"
        );
        // A ticket holds so many uploads.
        for _ in 1..UPLOADS_PER_TICKET {
            let added = store
                .add_upload(t, "x", "text/plain", b"x", &Party::you())
                .await
                .unwrap();
            assert!(matches!(added, Uploaded::Added(_)), "{added:?}");
        }
        assert!(matches!(
            store
                .add_upload(t, "one more", "text/plain", b"x", &Party::you())
                .await
                .unwrap(),
            Uploaded::TicketFull
        ));
    })
    .await;
}

/// A board's uploads together stay within their cap, across its tickets:
/// the one that would pass it is refused, and another board isn't held to it.
#[tokio::test]
async fn a_board_s_uploads_stay_within_their_cap() {
    with_test_pool(build_pool, |pool| async move {
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool);
        let one = a_ticket(&store).await;
        let board: Uuid = store
            .pool()
            .get()
            .await
            .unwrap()
            .query_one("SELECT board FROM tickets WHERE id = $1", &[&one])
            .await
            .unwrap()
            .get(0);
        let two = Uuid::new_v4();
        store
            .pool()
            .get()
            .await
            .unwrap()
            .execute(
                "INSERT INTO tickets (id, board, number, title, state, rank, created_by, state_at, created_at, updated_at) \
                 VALUES ($1, $2, 2, 'T2', 'todo', 2, '{\"kind\":\"you\",\"id\":\"you\",\"name\":\"You\"}', now(), now(), now())",
                &[&two, &board],
            )
            .await
            .unwrap();
        let other = a_ticket(&store).await;
        let add = |t: Uuid, n: usize| {
            let store = &store;
            async move {
                store
                    .add_upload_within(t, "f", "text/plain", &vec![1u8; n], &Party::you(), 1000)
                    .await
                    .unwrap()
            }
        };
        assert!(matches!(add(one, 600).await, Uploaded::Added(_)));
        assert!(matches!(add(two, 400).await, Uploaded::Added(_)), "up to the cap");
        assert!(matches!(add(two, 1).await, Uploaded::BoardFull), "past it, on any ticket of the board");
        assert!(matches!(add(other, 900).await, Uploaded::Added(_)), "another board's own");
    })
    .await;
}

/// An id too long to keep (an upload's address is its id) isn't listed:
/// one such file never stops the rest, or the issue's pull. A link's long
/// address is no id: the link is kept, with it, or past [`URL_MAX`] without.
#[test]
fn a_file_with_too_long_an_id_is_not_listed() {
    let long = format!(
        "https://uploads.linear.app/a1/{}.png",
        "x".repeat(REMOTE_MAX)
    );
    let body = format!("![huge]({long}) and ![ok](https://uploads.linear.app/a1/b2/ok.png)");
    let signed = format!("https://example.com/{}", "y".repeat(REMOTE_MAX));
    let huge = format!("https://example.com/{}", "z".repeat(URL_MAX));
    let issue = json!({ "attachments": { "nodes": [
        { "id": "att-1", "title": "long upload", "url": long },
        { "id": "att-2", "title": "long link", "url": signed },
        { "id": "att-3", "title": "Spec", "url": "https://example.com/spec" },
        { "id": "att-4", "title": "huge link", "url": huge },
    ]}});
    let files = linear_files(&issue, &body);
    let names: Vec<_> = files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["ok", "long link", "Spec", "huge link"]);
    assert_eq!(files[1].url.as_deref(), Some(signed.as_str()));
    assert_eq!(files[3].url, None, "past URL_MAX, kept without it");
}

/// A link whose address is long is kept whole; only an id is held to
/// [`REMOTE_MAX`].
#[tokio::test]
async fn a_link_s_long_address_is_kept() {
    with_test_pool(build_pool, |pool| async move {
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool);
        let t = a_ticket(&store).await;
        let url = format!(
            "https://bucket.s3.amazonaws.com/design.fig?X-Amz-Signature={}",
            "a".repeat(REMOTE_MAX * 2)
        );
        let link = RemoteFile {
            source: FileSource::LinearLink,
            remote_id: "att-1".into(),
            name: "Design".into(),
            media_type: "text/uri-list".into(),
            size: None,
            url: Some(url.clone()),
            thumb: false,
        };
        assert!(store
            .put_remote_files(t, &[link], &someone())
            .await
            .unwrap());
        let kept = &store.files_of(&[t]).await.unwrap()[&t];
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].url.as_deref(), Some(url.as_str()));
    })
    .await;
}

/// A file already here whose rewrite Postgres refuses goes, its kept bytes
/// with it: never left as it was, read from what it was.
#[tokio::test]
async fn a_file_whose_rewrite_fails_goes_with_its_bytes() {
    with_test_pool(build_pool, |pool| async move {
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool);
        let t = a_ticket(&store).await;
        let files = [remote("1", "a.png"), remote("2", "b.png")];
        assert!(store.put_remote_files(t, &files, &someone()).await.unwrap());
        // Now b's new name is one Postgres refuses.
        let files = [remote("1", "a.png"), remote("2", "b\0new.png")];
        assert!(store.put_remote_files(t, &files, &someone()).await.unwrap());
        let kept: Vec<_> = store.files_of(&[t]).await.unwrap()[&t]
            .iter()
            .map(|f| f.name.clone())
            .collect();
        assert_eq!(kept, ["a.png"]);
    })
    .await;
}

/// A file Postgres refuses, or one listed twice, is left out on its own:
/// the rest of the ticket's files go in, and the call doesn't fail.
#[tokio::test]
async fn one_bad_file_never_fails_a_ticket_s_files() {
    with_test_pool(build_pool, |pool| async move {
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool);
        let t = a_ticket(&store).await;
        let mut long = remote("3", "long.png");
        long.remote_id = "9".repeat(REMOTE_MAX + 1);
        let files = [
            remote("1", "a.png"),
            // Postgres takes no NUL in text: its insert fails.
            remote("2", "bad\0name.png"),
            remote("1", "a again.png"),
            long,
            remote("4", "d.png"),
        ];
        assert!(store.put_remote_files(t, &files, &someone()).await.unwrap());
        let kept: Vec<_> = store.files_of(&[t]).await.unwrap()[&t]
            .iter()
            .map(|f| f.name.clone())
            .collect();
        assert_eq!(kept, ["a.png", "d.png"]);
    })
    .await;
}

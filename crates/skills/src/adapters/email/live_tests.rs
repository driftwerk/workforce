//! Mail management against a real IMAP server (GreenMail or Dovecot on the
//! build box, never a person's mailbox): `EMAIL_IT_IMAP=host:port`, plain
//! IMAP, any user. Ignored unless asked for:
//! `EMAIL_IT_IMAP=127.0.0.1:3143 cargo test -p workforce-skills --features email -- --ignored live_`

use std::time::Duration;

use serde_json::json;
use tokio::net::TcpStream;

use super::bulk::{self, Budget};
use super::criteria::{Criteria, Selection};
use super::journal::memory::MemoryJournal;
use super::journal::{BulkAction, JobState, MailJournal, MarkFlag, Origin};
use super::mailbox::{Caps, Mailbox};
use super::manage::{folder_call, FolderOp};
use super::session::MailSession;

type Live = async_imap::Session<TcpStream>;

async fn login(user: &str) -> Live {
    let addr = std::env::var("EMAIL_IT_IMAP").expect("EMAIL_IT_IMAP=host:port");
    let stream = TcpStream::connect(addr).await.unwrap();
    let mut client = async_imap::Client::new(stream);
    client.read_response().await.unwrap().unwrap();
    client
        .login(user, "secret")
        .await
        .map_err(|(e, _)| e)
        .unwrap()
}

async fn seed(session: &mut Live, folder: &str, from: &str, n: usize, tag: &str) {
    for i in 0..n {
        let message = format!(
            "From: {from}\r\nTo: me@test\r\nSubject: {tag} {i}\r\nMessage-ID: <{tag}.{i}@test>\r\n\r\nbody {i}\r\n"
        );
        session
            .append(folder, None, None, message.as_bytes())
            .await
            .unwrap();
    }
}

fn user() -> String {
    format!("it{}@test", uuid::Uuid::new_v4().simple())
}

fn ask(selection: &Selection) -> bulk::Ask<'_> {
    bulk::Ask {
        owner: "kat",
        account: "it",
        tool: "email.move",
        origin: Origin::Person,
        selection,
        validity: None,
    }
}

fn from(sender: &str) -> Selection {
    Selection::Search(Criteria {
        from: Some(sender.to_string()),
        ..Criteria::default()
    })
}

#[tokio::test]
#[ignore = "needs EMAIL_IT_IMAP"]
async fn live_folders_are_made_nested_renamed_and_deleted_only_empty() {
    let mut session = login(&user()).await;
    let mailbox = Mailbox::probe(&mut session).await.unwrap();
    let made = folder_call(
        &mut session,
        &mailbox,
        FolderOp::Create,
        &json!({"folder": "Clients/Acme/2026"}),
    )
    .await
    .unwrap();
    assert_eq!(made["created"].as_array().unwrap().len(), 3, "{made}");
    let mailbox = Mailbox::probe(&mut session).await.unwrap();
    let acme = mailbox.server_name("Clients/Acme").unwrap();
    assert!(mailbox.get(&acme).is_some_and(|f| f.has_children));
    assert!(folder_call(
        &mut session,
        &mailbox,
        FolderOp::Delete,
        &json!({"folder": "Clients/Acme"})
    )
    .await
    .unwrap_err()
    .contains("folders inside it"));
    assert!(folder_call(
        &mut session,
        &mailbox,
        FolderOp::Delete,
        &json!({"folder": "INBOX"})
    )
    .await
    .is_err());
    let leaf = mailbox.server_name("Clients/Acme/2026").unwrap();
    seed(&mut session, &leaf, "a@x.com", 1, "keep").await;
    assert!(folder_call(
        &mut session,
        &mailbox,
        FolderOp::Delete,
        &json!({"folder": "Clients/Acme/2026"})
    )
    .await
    .unwrap_err()
    .contains("still holds 1 message"));
    folder_call(
        &mut session,
        &mailbox,
        FolderOp::Rename,
        &json!({"folder": "Clients/Acme/2026", "to": "Clients/Acme/Old"}),
    )
    .await
    .unwrap();
    let mailbox = Mailbox::probe(&mut session).await.unwrap();
    let old = mailbox.server_name("Clients/Acme/Old").unwrap();
    assert_eq!(
        MailSession::status(&mut session, &old)
            .await
            .unwrap()
            .messages,
        1
    );
    let empty = mailbox.server_name("Clients/Empty").unwrap();
    session.create(&empty).await.unwrap();
    let mailbox = Mailbox::probe(&mut session).await.unwrap();
    folder_call(
        &mut session,
        &mailbox,
        FolderOp::Delete,
        &json!({"folder": "Clients/Empty"}),
    )
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs EMAIL_IT_IMAP"]
async fn live_two_thousand_messages_move_over_several_calls_and_undo() {
    let mut session = login(&user()).await;
    seed(&mut session, "INBOX", "news@shop.com", 2_000, "news").await;
    seed(&mut session, "INBOX", "friend@x.com", 7, "friend").await;
    session.create("News").await.unwrap();
    let mailbox = Mailbox::probe(&mut session).await.unwrap();
    assert!(
        mailbox.caps.moves_safely(),
        "the test server can move safely: {:?}",
        mailbox.caps
    );

    let plan = bulk::plan(
        &mut session,
        &mailbox,
        "INBOX",
        &from("news@shop.com"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(plan["count"], 2_000);
    assert_eq!(plan["sample"].as_array().unwrap().len(), 10);
    assert_eq!(
        MailSession::status(&mut session, "INBOX")
            .await
            .unwrap()
            .messages,
        2_007
    );

    let journal = MemoryJournal::default();
    let action = BulkAction::Move {
        from: "INBOX".to_string(),
        to: "News".to_string(),
        label: None,
    };
    let mut job = bulk::begin(&mut session, &mailbox, action, ask(&from("news@shop.com")))
        .await
        .unwrap();
    let mut calls = 0;
    while job.state == JobState::Running {
        calls += 1;
        let budget = Budget::of(Duration::from_millis(if calls == 1 { 1 } else { 60_000 }));
        bulk::run(&mut session, &mailbox, &mut job, budget, Some(&journal))
            .await
            .unwrap();
        job = journal.get("kat", &job.id).await.unwrap().unwrap();
    }
    assert!(
        calls >= 2,
        "the first call ran out of time and the job carried on"
    );
    assert_eq!(job.state, JobState::Done);
    assert_eq!(
        MailSession::status(&mut session, "News")
            .await
            .unwrap()
            .messages,
        2_000
    );
    assert_eq!(
        MailSession::status(&mut session, "INBOX")
            .await
            .unwrap()
            .messages,
        7
    );

    bulk::undo(
        &mut session,
        &mailbox,
        &mut job,
        Budget::of(Duration::from_secs(120)),
        Some(&journal),
    )
    .await
    .unwrap();
    assert_eq!(job.state, JobState::Undone);
    assert_eq!(job.unfound, 0);
    assert_eq!(
        MailSession::status(&mut session, "INBOX")
            .await
            .unwrap()
            .messages,
        2_007
    );
    assert_eq!(
        MailSession::status(&mut session, "News")
            .await
            .unwrap()
            .messages,
        0
    );
}

#[tokio::test]
#[ignore = "needs EMAIL_IT_IMAP"]
async fn live_a_mark_and_its_undo_change_exactly_what_they_should() {
    let mut session = login(&user()).await;
    seed(&mut session, "INBOX", "news@shop.com", 30, "mark").await;
    let mailbox = Mailbox::probe(&mut session).await.unwrap();
    session.select("INBOX").await.unwrap();
    let first: super::uids::UidSet = MailSession::search(&mut session, "ALL")
        .await
        .unwrap()
        .iter()
        .take(5)
        .collect();
    MailSession::store(&mut session, &first, "+FLAGS.SILENT (\\Seen)")
        .await
        .unwrap();
    let action = BulkAction::Mark {
        folder: "INBOX".to_string(),
        flag: MarkFlag::Seen,
        on: true,
    };
    let mut job = bulk::begin(&mut session, &mailbox, action, ask(&from("news@shop.com")))
        .await
        .unwrap();
    bulk::run(
        &mut session,
        &mailbox,
        &mut job,
        Budget::of(Duration::from_secs(60)),
        None,
    )
    .await
    .unwrap();
    assert_eq!(job.handled(), 25);
    session.select("INBOX").await.unwrap();
    assert_eq!(
        MailSession::search(&mut session, "UNSEEN")
            .await
            .unwrap()
            .len(),
        0
    );
    bulk::undo(
        &mut session,
        &mailbox,
        &mut job,
        Budget::of(Duration::from_secs(60)),
        None,
    )
    .await
    .unwrap();
    session.select("INBOX").await.unwrap();
    assert_eq!(
        MailSession::search(&mut session, "SEEN").await.unwrap(),
        first
    );
}

async fn moved_beside_a_deleted_message(without_move: bool) {
    let mut session = login(&user()).await;
    seed(&mut session, "INBOX", "news@shop.com", 3, "news").await;
    seed(&mut session, "INBOX", "friend@x.com", 1, "pending").await;
    session.create("News").await.unwrap();
    let probed = Mailbox::probe(&mut session).await.unwrap();
    let mailbox = Mailbox {
        caps: Caps {
            moves: probed.caps.moves && !without_move,
            ..probed.caps
        },
        ..probed
    };
    assert!(mailbox.caps.moves_safely(), "{:?}", mailbox.caps);
    session.select("INBOX").await.unwrap();
    let pending = MailSession::search(&mut session, "FROM \"friend@x.com\"")
        .await
        .unwrap();
    MailSession::store(&mut session, &pending, "+FLAGS.SILENT (\\Deleted)")
        .await
        .unwrap();
    let action = BulkAction::Move {
        from: "INBOX".to_string(),
        to: "News".to_string(),
        label: None,
    };
    let mut job = bulk::begin(&mut session, &mailbox, action, ask(&from("news@shop.com")))
        .await
        .unwrap();
    bulk::run(
        &mut session,
        &mailbox,
        &mut job,
        Budget::of(Duration::from_secs(60)),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        MailSession::status(&mut session, "INBOX")
            .await
            .unwrap()
            .messages,
        1
    );
    assert_eq!(
        MailSession::status(&mut session, "News")
            .await
            .unwrap()
            .messages,
        3
    );
    bulk::undo(
        &mut session,
        &mailbox,
        &mut job,
        Budget::of(Duration::from_secs(60)),
        None,
    )
    .await
    .unwrap();
    assert_eq!(job.state, JobState::Undone);
    assert_eq!(
        MailSession::status(&mut session, "INBOX")
            .await
            .unwrap()
            .messages,
        4
    );
    assert_eq!(
        MailSession::status(&mut session, "News")
            .await
            .unwrap()
            .messages,
        0
    );
}

#[tokio::test]
#[ignore = "needs EMAIL_IT_IMAP"]
async fn live_a_move_never_expunges_another_message_marked_deleted() {
    moved_beside_a_deleted_message(false).await;
}

#[tokio::test]
#[ignore = "needs EMAIL_IT_IMAP"]
async fn live_without_move_the_copy_fallback_takes_only_its_own_messages() {
    moved_beside_a_deleted_message(true).await;
}

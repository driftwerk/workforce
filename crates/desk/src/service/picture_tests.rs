//! The picture as the desk feeds it: the person's prompt into a session,
//! their message to it from Desk, and what the session says on its line,
//! each there the moment it happens; a Desk message Desk's plugin hands
//! in is not the person's; and the feed outlives a restart.

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::super::tests::{desk, new_session};
use super::*;
use crate::line::When;
use crate::store::build_pool;

async fn hook(d: &Desk, id: Uuid, body: Value) -> Value {
    let token = d.0.state.lock().live[&id].token.clone();
    d.hook(id, &token, body.to_string().as_bytes())
        .await
        .unwrap()
}

fn words(d: &Desk, id: Uuid) -> Vec<(What, String)> {
    d.picture_recent("any-boss", id, 20)
        .into_iter()
        .map(|r| (r.what, r.text))
        .collect()
}

#[tokio::test]
async fn what_the_person_types_into_a_session_is_in_the_picture_within_a_second() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.picture_load().await;
        let a = new_session(&d, dir.path()).await;
        hook(
            &d,
            a.id,
            json!({"hook_event_name": "SessionStart", "session_id": a.id.to_string()}),
        )
        .await;
        d.plugin_heard_from(a.id);
        let before = d.picture_version("any-boss", a.id);
        let typed = Instant::now();
        hook(
            &d,
            a.id,
            json!({"hook_event_name": "UserPromptSubmit",
                "prompt": "Don't write your own fix: Sam owns it (#648)."}),
        )
        .await;
        // There as the hook returns: no wait, no poll.
        assert!(typed.elapsed() < Duration::from_secs(1));
        assert!(d.picture_version("any-boss", a.id) > before);
        assert_eq!(
            words(&d, a.id),
            vec![(
                What::PersonSaid,
                "Don't write your own fix: Sam owns it (#648).".to_string()
            )]
        );
        // A message Desk's plugin handed in is Desk's, not the person's.
        let m = d
            .line_post(a.id, "Review #648 read-only.".into(), When::Queue)
            .await
            .unwrap();
        hook(
            &d,
            a.id,
            json!({"hook_event_name": "UserPromptSubmit",
                "prompt": format!("The desk plugin sent a message: {} Review #648 read-only.", crate::line::marker(m.id))}),
        )
        .await;
        let said = words(&d, a.id);
        assert_eq!(said.len(), 2, "{said:?}");
        assert_eq!(
            said[1],
            (What::PersonSaid, "Review #648 read-only.".to_string()),
            "the person's message from Desk, once"
        );
        // The person's message typed in by Desk (pasted at its prompt) is
        // theirs once, from when they wrote it, not again as it goes in.
        let m = d
            .line_post(a.id, "Then run the tests.".into(), When::Queue)
            .await
            .unwrap();
        d.breaker_typed(a.id, &m.text, Some(&crate::crew::By::you()));
        hook(
            &d,
            a.id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": m.text}),
        )
        .await;
        let said = words(&d, a.id);
        assert_eq!(said.len(), 3, "{said:?}");
        // What Desk writes on the person's side isn't their words.
        d.line_send(
            crate::service::Sender::Person,
            crate::line::Send::Tell {
                to: crate::line::Target::Session(a.id),
                text: "ACM-12 was moved to Done; it is no longer yours.".into(),
                when: When::Queue,
            },
        )
        .await
        .unwrap();
        assert_eq!(words(&d, a.id).len(), 3);
        // What the session tells its boss on its line is there too.
        d.line_call(
            a.id,
            "tell",
            &json!({"to": "you", "text": "Reviewed #648: it handles both id forms."}),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            words(&d, a.id).last().unwrap(),
            &(
                What::SessionReported,
                "Reviewed #648: it handles both id forms.".to_string()
            )
        );
    })
    .await;
}

#[tokio::test]
async fn the_picture_outlives_a_restart_and_numbering_goes_on() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = Uuid::new_v4();
        // Said before the picture was read back: kept after what was.
        let early = d
            .picture_note(Note::new(What::Fact, "Sam owns the fix (#648).").boss("boss"))
            .unwrap();
        d.picture_load().await;
        let next = d
            .picture_note(Note::new(What::PersonSaid, "Pause merges.").session(s))
            .unwrap();
        assert!(next.seq > early.seq);
        // Written as they happen.
        for _ in 0..200 {
            let (rows, _) = d.store().picture_load(Utc::now()).await.unwrap();
            if rows.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Read back as a restarted desk reads it.
        let (rows, top) = d.store().picture_load(Utc::now()).await.unwrap();
        let mut again = crate::picture::Picture::default();
        assert!(again.load(rows, top).is_empty());
        let kept: Vec<String> = again
            .recent("boss", s, 10)
            .into_iter()
            .map(|r| r.text)
            .collect();
        assert_eq!(kept, vec!["Sam owns the fix (#648).", "Pause merges."]);
        assert_eq!(again.seq(), next.seq);
    })
    .await;
}

#[tokio::test]
async fn a_boss_message_the_persons_own_words_overtook_is_withdrawn_never_delivered() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.picture_load().await;
        let a = new_session(&d, dir.path()).await;
        let view = d.view(a.id).unwrap();
        let queued = |text: &str| {
            let mut m = crate::line::LineMessage::new(
                crate::crew::Party::manager("boss", "Boss"),
                crate::crew::Party::session(&view),
                crate::line::LineKind::Tell,
                text.into(),
                1,
            );
            m.delivery = crate::line::Delivery::Queued;
            m
        };
        // The boss's message waits for the session's turn to end...
        let before = queued("Open the PR for the picker.");
        d.store().put_line(&before).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        // ...and the person, in the session, says otherwise.
        d.picture_person_said(a.id, "Don't open a PR: I'm reworking the picker.");
        tokio::time::sleep(Duration::from_millis(5)).await;
        // What the boss decides after their words stands.
        let after = queued("Understood: no PR. Review the rework when they hand it back.");
        d.store().put_line(&after).await.unwrap();
        d.line_pump(a.id).await;
        let delivery = |id| {
            let d = d.clone();
            async move { d.store().line_message(id).await.unwrap().unwrap().delivery }
        };
        assert_eq!(delivery(before.id).await, crate::line::Delivery::Withdrawn);
        assert_ne!(delivery(after.id).await, crate::line::Delivery::Withdrawn);
        // Its boss's picture says it never went in, and it is no longer
        // the instruction the session carries out.
        let rows = d.picture_recent("boss", a.id, 10);
        assert!(
            rows.iter()
                .any(|r| r.what == What::Withdrawn && r.text == before.text),
            "{rows:?}"
        );
        d.picture_note(
            Note::new(What::Instruction, before.text.clone())
                .boss("boss")
                .session(a.id),
        );
        d.picture_note(
            Note::new(What::Withdrawn, before.text.clone())
                .boss("boss")
                .session(a.id),
        );
        assert!(d.picture_last_instruction("boss", a.id).is_none());

        // The person answering its question from Desk isn't them taking
        // its wheel: a boss's message waiting overtakes nothing.
        let waiting = queued("Then run the tests.");
        d.store().put_line(&waiting).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        d.picture_person_answered(a.id, "Use port 5433.");
        d.line_pump(a.id).await;
        assert_ne!(delivery(waiting.id).await, crate::line::Delivery::Withdrawn);

        // The boss's answer to the question its turn ended on, still
        // waiting when the person answered it there themselves: withdrawn.
        let mut answer = crate::line::LineMessage::new(
            crate::crew::Party::manager("boss", "Boss"),
            crate::crew::Party::session(&view),
            crate::line::LineKind::Answer,
            "Yes, ship it.".into(),
            1,
        );
        answer.delivery = crate::line::Delivery::Queued;
        d.store().put_line(&answer).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        d.picture_person_said(a.id, "No, hold the release.");
        d.line_pump(a.id).await;
        assert_eq!(delivery(answer.id).await, crate::line::Delivery::Withdrawn);
    })
    .await;
}

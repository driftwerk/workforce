//! The same instruction to one session, again and again (15 Nov: seven
//! times in two minutes): sent once, decided once; the boss is told it
//! already said it, and when.

use super::super::actions::{Action, Target};
use super::*;

const ACT: &str = "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n";

#[tokio::test(flavor = "multi_thread")]
async fn the_same_instruction_to_a_session_is_sent_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        let other = new_session(&d, dir.path(), "acme-api").await;
        let nudge = |to: uuid::Uuid, message: &str| Action::Nudge {
            to: Target::Session(to),
            message: message.into(),
            reply_to: None,
        };
        let by = || crew.by_overseer("It stopped moving.").unwrap();
        let first = crew
            .perform(&nudge(s.id, "Rebase onto main and push."), by())
            .await
            .unwrap();
        assert!(first.starts_with("told acme-web"), "{first}");
        for again in [
            "Rebase onto main and push.",
            "rebase onto main and push",
            "Rebase  onto main and push!",
            "Rebase onto main and push.",
            "Rebase onto main and push.",
            "Rebase onto main and push.",
        ] {
            let said = crew.perform(&nudge(s.id, again), by()).await.unwrap();
            assert!(said.starts_with("already told acme-web that at "), "{said}");
            assert!(said.contains("not sent again"), "{said}");
        }
        let rows = d
            .store()
            .decisions_since("desk", chrono::DateTime::UNIX_EPOCH, 50)
            .await
            .unwrap();
        let on_it = rows.iter().filter(|r| r.session == Some(s.id)).count();
        assert_eq!(on_it, 1, "one decision for the seven sends: {rows:?}");
        // Something new, or the same to another session, goes.
        let new = crew
            .perform(&nudge(s.id, "Then run the tests."), by())
            .await
            .unwrap();
        assert!(new.starts_with("told acme-web"), "{new}");
        let elsewhere = crew
            .perform(&nudge(other.id, "Rebase onto main and push."), by())
            .await
            .unwrap();
        assert!(elsewhere.starts_with("told acme-api"), "{elsewhere}");

        // Two at once, in one turn: still sent once.
        let open_pr = nudge(other.id, "Open the PR.");
        let (a, b) = tokio::join!(crew.perform(&open_pr, by()), crew.perform(&open_pr, by()));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(
            [&a, &b].iter().filter(|x| x.starts_with("told acme-api")).count(),
            1,
            "{a} / {b}"
        );
        assert!([&a, &b].iter().any(|x| x.starts_with("already told acme-api")));

        // Once the session has answered since, the same words are fair:
        // "carry on" to one that moved on and stopped again.
        let url = hook_url(dir.path(), s.id).await;
        hook(
            &url,
            json!({"hook_event_name": "SessionStart", "session_id": "cc-acme-web"}),
        )
        .await;
        hook(
            &url,
            json!({"hook_event_name": "Stop", "last_assistant_message": "Rebased; the push was rejected."}),
        )
        .await;
        eventually("its reply", async || {
            d.view(s.id).ok().and_then(|v| v.last_reply).filter(|r| r.contains("rejected"))
        })
        .await;
        let after = crew
            .perform(&nudge(s.id, "Rebase onto main and push."), by())
            .await
            .unwrap();
        assert!(after.starts_with("told acme-web"), "{after}");

        // The person telling it the same is never held back.
        let person = crew
            .perform(&nudge(s.id, "Rebase onto main and push."), By::you())
            .await
            .unwrap();
        assert!(person.starts_with("told acme-web"), "{person}");
    })
    .await;
}

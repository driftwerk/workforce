//! The boss's decision log (overseer H4): what it does is recorded as it
//! does it, with how it is undone; a turn that does nothing is recorded with
//! what it said; an Undo takes back a message still waiting and corrects one
//! gone in, once; and none of it spends the check-in budget.

use super::super::actions::{Action, Target};
use super::*;
use workforce_desk::crew::UndoRefused;
use workforce_desk::decision::{How, Need, Undo};
use workforce_desk::line::Delivery;

fn epoch() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::UNIX_EPOCH
}

/// A boss's message waiting on a session's line is taken back by Undo, and
/// once only; one already gone in is corrected instead.
#[tokio::test(flavor = "multi_thread")]
async fn an_undo_withdraws_a_waiting_message_and_corrects_one_gone_in() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        let checkins = d.store().wake_ups_since("desk", epoch()).await.unwrap();
        let nudge = Action::Nudge {
            to: Target::Session(s.id),
            message: "Wait for green CI before you merge.".into(),
            reply_to: None,
        };
        crew.perform(&nudge, crew.by_overseer("CI is red.").unwrap())
            .await
            .unwrap();
        let decisions = d
            .store()
            .decisions_since("desk", epoch(), 10)
            .await
            .unwrap();
        assert_eq!(decisions.len(), 1, "{decisions:?}");
        let first = decisions[0].clone();
        assert_eq!(first.session, Some(s.id));
        assert_eq!(first.how, How::Own);
        assert_eq!(first.need, Need::Choice);
        assert!(first.why.starts_with("CI is red."), "{}", first.why);
        assert!(
            first.what.contains("Wait for green CI"),
            "said from the action: {}",
            first.what
        );
        assert_eq!(
            d.store().wake_ups_since("desk", epoch()).await.unwrap(),
            checkins,
            "recording spends no check-in"
        );
        let Some(workforce_desk::decision::UndoPlan::Line { message, .. }) = first.plan() else {
            panic!("a line message's undo: {:?}", first.action);
        };
        let sent = d.store().line_message(message).await.unwrap().unwrap();
        assert!(
            matches!(sent.delivery, Delivery::Queued | Delivery::Held),
            "{:?}",
            sent.delivery
        );
        assert_eq!(first.undo, Undo::Withdraw);
        // Undo while it waits: taken back, never to go in.
        let done = crew.undo(first.id).await.unwrap();
        assert!(done.contains("took back"), "{done}");
        let now = d.store().line_message(message).await.unwrap().unwrap();
        assert_eq!(now.delivery, Delivery::Withdrawn);
        assert_eq!(crew.undo(first.id).await, Err(UndoRefused::Already));
        // The undo is a decision of its own, the person's.
        let all = d
            .store()
            .decisions_since("desk", epoch(), 10)
            .await
            .unwrap();
        let undid = all.iter().find(|x| x.how == How::PersonFirst).unwrap();
        assert_eq!(
            undid.need_ref.as_deref(),
            Some(first.id.to_string().as_str())
        );
        assert!(d
            .store()
            .decision(first.id)
            .await
            .unwrap()
            .unwrap()
            .undone_at
            .is_some());

        // One that already went in is corrected: the person tells it.
        crew.perform(&nudge, crew.by_overseer("CI is red again.").unwrap())
            .await
            .unwrap();
        let second = d
            .store()
            .decisions_since("desk", epoch(), 10)
            .await
            .unwrap()
            .into_iter()
            .find(|x| x.why.starts_with("CI is red again."))
            .unwrap();
        let Some(workforce_desk::decision::UndoPlan::Line { message, .. }) = second.plan() else {
            panic!();
        };
        let mut gone_in = d.store().line_message(message).await.unwrap().unwrap();
        gone_in.delivery = Delivery::Delivered;
        gone_in.delivered_at = Some(chrono::Utc::now());
        d.store().put_line(&gone_in).await.unwrap();
        let done = crew.undo(second.id).await.unwrap();
        assert!(done.contains("disregard"), "{done}");
        let inbox = d.store().line_inbox(&s.id.to_string()).await.unwrap();
        assert!(
            inbox.iter().any(|m| m.from.kind == PartyKind::You
                && m.text.contains("disregard Desk's earlier message")),
            "{inbox:?}"
        );
    })
    .await;
}

/// What ran is fixed forward: a decision with no undo is refused, as an
/// unknown one is and one already undone.
#[tokio::test(flavor = "multi_thread")]
async fn an_undo_refuses_what_cant_be_undone() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        let approved = workforce_desk::decision::BossDecision::new(
            "desk",
            Need::Approval,
            "allowed acme-web to run its tests",
            "A read-only test run.",
            How::Own,
        );
        d.store().decision_put(&approved).await.unwrap();
        assert!(matches!(
            crew.undo(approved.id).await,
            Err(UndoRefused::Cant(_))
        ));
        assert!(
            d.store()
                .decision(approved.id)
                .await
                .unwrap()
                .unwrap()
                .undone_at
                .is_none(),
            "a refused undo marks nothing"
        );
        assert_eq!(
            crew.undo(uuid::Uuid::new_v4()).await,
            Err(UndoRefused::NotFound)
        );
        let mut done = approved.clone();
        done.id = uuid::Uuid::new_v4();
        done.undo = Undo::Reverse;
        done.action = workforce_desk::decision::UndoPlan::GoAhead {
            session: uuid::Uuid::new_v4(),
        }
        .value();
        done.undone_at = Some(chrono::Utc::now());
        d.store().decision_put(&done).await.unwrap();
        assert_eq!(crew.undo(done.id).await, Err(UndoRefused::Already));
    })
    .await;
}

/// A work turn that does nothing is a decision: "let it wait", with what
/// the boss said as why, and an Undo that tells the session to go ahead.
#[tokio::test(flavor = "multi_thread")]
async fn a_work_turn_that_does_nothing_is_recorded_with_its_reason() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "CI is still running, so it should wait. Nothing else to do.",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        let url = hook_url(dir.path(), s.id).await;
        hook(
            &url,
            json!({"hook_event_name": "SessionStart", "session_id": s.id.to_string()}),
        )
        .await;
        hook(
            &url,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "Upgrade the date picker"}),
        )
        .await;
        let boss = workforce_desk::crew::Party::manager("desk", &super::super::name_of(&overseer));
        d.take_on(
            s.id,
            &boss,
            workforce_desk::charge::How::Handed,
            &Default::default(),
            &By::you(),
        )
        .await
        .unwrap();
        hook(
            &url,
            json!({"hook_event_name": "Stop", "last_assistant_message": "Pushed; CI is running."}),
        )
        .await;
        let waited = eventually("the turn's decision", async || {
            d.store()
                .decisions_since("desk", epoch(), 10)
                .await
                .unwrap()
                .into_iter()
                .find(|x| x.need == Need::TurnEnd)
        })
        .await;
        assert_eq!(waited.what, "let acme-web wait");
        assert_eq!(
            waited.why, "CI is still running, so it should wait.",
            "its reply's own words"
        );
        assert_eq!(waited.session, Some(s.id));
        assert_eq!(waited.button(), Some("undo"));
        // The prompt asked it to say why when it does nothing.
        let channel = super::super::line::line_channel(s.id);
        let line = overseer.sessions.find(&channel, PERSON).unwrap().unwrap();
        assert!(line.messages.iter().any(|m| m
            .content
            .contains("If you do nothing, say why in one sentence.")));
        let done = crew.undo(waited.id).await.unwrap();
        assert_eq!(done, "told acme-web to go ahead");
        let inbox = d.store().line_inbox(&s.id.to_string()).await.unwrap();
        assert!(inbox
            .iter()
            .any(|m| m.text == "Go ahead with your next step."));
    })
    .await;
}

/// `crew.decisions` reads the boss's own log, grouped by session, with how
/// many needs it handled itself.
#[tokio::test(flavor = "multi_thread")]
async fn crew_decisions_reads_the_log_with_the_handled_count() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        for (need, how, what) in [
            (Need::Approval, How::Own, "allowed acme-web's test run"),
            (
                Need::Question,
                How::Escalated,
                "gave you acme-web's question",
            ),
        ] {
            let mut x =
                workforce_desk::decision::BossDecision::new("desk", need, what, "Because.", how);
            x.session = Some(s.id);
            d.store().decision_put(&x).await.unwrap();
        }
        let other = workforce_desk::decision::BossDecision::new(
            "another-boss",
            Need::Approval,
            "not this boss's",
            "",
            How::Own,
        );
        d.store().decision_put(&other).await.unwrap();
        let tool =
            super::super::tools::CrewTool::for_level(&crew, "desk", &Default::default(), false)
                .into_iter()
                .find(|t| workforce_agent::tools::Tool::name(t) == "crew.decisions")
                .unwrap();
        let out = workforce_agent::tools::Tool::execute(&tool, json!({}))
            .await
            .unwrap()
            .output;
        assert_eq!(out["handled"]["needs"], 2, "{out}");
        assert_eq!(out["handled"]["yourself"], 1, "{out}");
        let text = out["decisions"].as_str().unwrap();
        assert!(text.contains("acme-web (session_id"), "{text}");
        assert!(
            text.contains("allowed acme-web's test run — Because. [own]"),
            "{text}"
        );
        assert!(!text.contains("not this boss's"), "{text}");
    })
    .await;
}

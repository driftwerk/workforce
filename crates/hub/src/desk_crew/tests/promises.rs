//! The boss's promises and the person's orders (overseer H6): kept with the
//! sessions they are about until those sessions' accounts show them done,
//! a status question's promise kept by the session's reply, an order from
//! a DM turn that acted on sessions, and each lapsing when its sessions end
//! or nothing changes for 3 days.

use super::super::actions::{Action, Target, STATUS_QUESTION};
use super::*;
use workforce_desk::open_item::{ClosedBy, OpenKind, OpenState};

fn tool(crew: &Arc<CrewHub>, name: &str) -> super::super::tools::CrewTool {
    super::super::tools::CrewTool::for_level(crew, "desk", &Default::default(), false)
        .into_iter()
        .find(|t| workforce_agent::tools::Tool::name(t) == name)
        .unwrap()
}

async fn call(crew: &Arc<CrewHub>, name: &str, input: Value) -> workforce_agent::tools::ToolResult {
    workforce_agent::tools::Tool::execute(&tool(crew, name), input)
        .await
        .unwrap()
}

fn said(overseer: &AgentService, starts: &str) -> Option<String> {
    overseer
        .sessions
        .find(DESK_CHANNEL, PERSON)
        .unwrap()?
        .messages
        .into_iter()
        .find(|m| m.content.starts_with(starts))
        .map(|m| m.content)
}

const ACT: &str = "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n";

/// An order the boss acts on from the person's message is kept with the
/// sessions it acted on; one with an end is done, and reported once, when
/// a linked session's account shows it.
#[tokio::test(flavor = "multi_thread")]
async fn an_order_links_its_sessions_and_is_done_when_the_account_shows_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        let started = chrono::Utc::now();
        crew.perform(
            &Action::Nudge {
                to: Target::Session(s.id),
                message: "Add the lead picker endpoint.".into(),
                reply_to: None,
            },
            crew.by_overseer("The person asked for it.").unwrap(),
        )
        .await
        .unwrap();
        crew.order_from_turn(&overseer, "no-such-session", started, "Add the lead picker endpoint")
            .await;
        let order = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.kind == OpenKind::Order)
            .expect("the order");
        assert_eq!(order.sessions, vec![s.id]);
        assert_eq!(order.text, "Add the lead picker endpoint");
        let block = crew.items_context("desk").await.unwrap();
        assert!(block.contains("The person's orders"), "{block}");

        // One with an end, through crew.order.
        let r = call(
            &crew,
            super::super::items::ORDER,
            json!({ "text": "Ship the picker fix", "session_ids": [s.id.to_string()], "done_when": "merged" }),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let mut a = workforce_desk::account::Account::new(s.id, "", chrono::Utc::now());
        a.done.push(workforce_desk::account::Done {
            text: "Merged #622 after CI went green".into(),
            refs: vec![],
            at: chrono::Utc::now(),
        });
        d.store().session_account_put(&a).await.unwrap();
        crew.look_at_items().await;
        let kept = d
            .store()
            .items_closed_since("desk", started, 10)
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.text == "Ship the picker fix")
            .expect("kept");
        assert_eq!(kept.closed_by, Some(ClosedBy::Boss));
        assert_eq!(kept.closed_why.as_deref(), Some("Done: Merged #622 after CI went green"));
        let report = eventually("the report back", async || {
            said(&overseer, "The change you asked for at")
        })
        .await;
        assert!(report.contains("is merged: Merged #622"), "{report}");
        // The order with no end stays.
        assert_eq!(
            d.store().open_item(order.id).await.unwrap().unwrap().state,
            OpenState::Open
        );
    })
    .await;
}

/// Asking a session where it is promises the person its reply: kept, and
/// said once, when the session answers on its line.
#[tokio::test(flavor = "multi_thread")]
async fn a_status_question_is_a_promise_kept_by_the_sessions_reply() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        crew.perform(
            &Action::Nudge {
                to: Target::Session(s.id),
                message: format!("{STATUS_QUESTION} Which API key did you use?"),
                reply_to: None,
            },
            crew.by_overseer("The person asked.").unwrap(),
        )
        .await
        .unwrap();
        let promise = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.kind == OpenKind::Promise)
            .expect("the promise");
        assert!(
            promise.text.contains("Which API key did you use?"),
            "{}",
            promise.text
        );
        assert!(promise.line_message.is_some());
        d.line_call(
            s.id,
            "tell",
            &json!({"to": "desk", "text": "The staging key, STRIPE_TEST."}),
            None,
        )
        .await
        .unwrap();
        let closed = eventually("kept by the reply", async || {
            d.store()
                .open_item(promise.id)
                .await
                .unwrap()
                .filter(|i| i.state == OpenState::Closed)
        })
        .await;
        assert_eq!(closed.closed_by, Some(ClosedBy::Boss));
        let report = eventually("the reply said", async || {
            said(&overseer, "acme-web answered what I asked it at")
        })
        .await;
        assert!(report.contains("The staging key"), "{report}");
    })
    .await;
}

/// A promise lapses once its session has ended, said once.
#[tokio::test(flavor = "multi_thread")]
async fn a_promise_lapses_when_its_session_ends() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        let r = call(
            &crew,
            super::super::items::PROMISE,
            json!({ "session_id": s.id.to_string(), "text": "Tell you when the fix is live", "done_when": "in production" }),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        d.forget(s.id).await.unwrap();
        crew.look_at_items().await;
        let lapsed = d
            .store()
            .items_closed_since("desk", chrono::Utc::now() - chrono::Duration::hours(1), 10)
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.kind == OpenKind::Promise)
            .expect("lapsed");
        assert_eq!(lapsed.closed_by, Some(ClosedBy::Lapsed));
        assert_eq!(lapsed.closed_why.as_deref(), Some("its session ended"));
        eventually("said once", async || {
            said(&overseer, "I'm no longer tracking “Tell you when the fix is live”")
        })
        .await;
    })
    .await;
}

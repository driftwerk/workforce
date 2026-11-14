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
        let other = new_session(&d, dir.path(), "acme-api").await;
        let asked = new_session(&d, dir.path(), "acme-docs").await;
        let started = chrono::Utc::now();
        let nudge = |to: uuid::Uuid, message: &str| Action::Nudge {
            to: Target::Session(to),
            message: message.into(),
            reply_to: None,
        };
        let in_turn = |turn: &'static str, action: Action| {
            let crew = crew.clone();
            workforce_agent::runner::CURRENT_SESSION_ID.scope(turn.to_string(), async move {
                crew.perform(&action, crew.by_overseer("The person asked for it.").unwrap())
                    .await
                    .unwrap();
            })
        };
        // What this turn did for the person; a question it asked, and
        // another turn's work at the same time, aren't.
        in_turn("dm-turn", nudge(s.id, "Add the lead picker endpoint.")).await;
        in_turn("dm-turn", nudge(asked.id, "Where are the docs up to?")).await;
        in_turn("work-turn", nudge(other.id, "Rebase onto main.")).await;
        // Nothing done in a turn is an order of nobody's.
        crew.order_from_turn(&overseer, "quiet-turn", started, "what's up?")
            .await;
        crew.order_from_turn(&overseer, "dm-turn", started, "Add the lead picker endpoint")
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
        assert_eq!(
            d.store()
                .items_not_closed(Some("desk"))
                .await
                .unwrap()
                .iter()
                .filter(|i| i.kind == OpenKind::Order)
                .count(),
            1,
            "only the turn that acted keeps an order"
        );
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
        let pr = workforce_desk::account::Ref {
            kind: workforce_desk::account::RefKind::Pr,
            id: "#622".into(),
            url: None,
        };
        // Another merge isn't the change merged.
        a.done.push(workforce_desk::account::Done {
            text: "Merged main into the branch".into(),
            refs: vec![pr.clone()],
            at: chrono::Utc::now(),
        });
        let workforce_desk::account_store::Put::Kept(v) = d.store().session_account_put(&a).await.unwrap()
        else {
            panic!("the account kept");
        };
        a.version = v;
        crew.look_at_items().await;
        assert!(
            d.store()
                .items_not_closed(Some("desk"))
                .await
                .unwrap()
                .iter()
                .any(|i| i.text == "Ship the picker fix"),
            "merging main in keeps nothing"
        );
        a.done.push(workforce_desk::account::Done {
            text: "Merged #622 after CI went green".into(),
            refs: vec![pr],
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
        // Reported once, in the boss's next update.
        let report = eventually("the report back", async || {
            overseer
                .sessions
                .find(DESK_CHANNEL, PERSON)
                .unwrap()?
                .messages
                .into_iter()
                .find(|m| m.content.contains("✓ Merged #622 after CI went green"))
                .map(|m| m.content)
        })
        .await;
        assert!(
            !report.contains("Ship the picker fix") && !report.contains("The change you asked for"),
            "one short line, never the order said back: {report}"
        );
        assert!(
            said(&overseer, "✓ Merged #622").is_none(),
            "not as a post of its own"
        );
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
        // An answer to something else it was asked doesn't keep it.
        let mut elsewhere = workforce_desk::line::LineMessage::new(
            workforce_desk::crew::Party::session(&d.view(s.id).unwrap()),
            workforce_desk::crew::Party {
                kind: PartyKind::Manager,
                id: "desk".into(),
                name: "Scout".into(),
                engine: None,
                origin: None,
            },
            workforce_desk::line::LineKind::Answer,
            "Yes, go ahead.".into(),
            0,
        );
        elsewhere.in_reply_to = Some(uuid::Uuid::new_v4());
        crew.replied(&elsewhere).await;
        assert_eq!(
            d.store()
                .open_item(promise.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            OpenState::Open,
            "an answer to another question"
        );
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

/// As on 11/14: the same order said twice is one item; orders closed as a
/// duplicate or because the person came back say nothing in the update,
/// and one closed with what was done is one short "✓" line, never the
/// person's words said back.
#[tokio::test(flavor = "multi_thread")]
async fn orders_that_simply_ended_say_nothing_and_the_same_order_twice_is_one() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        crew.look_at_updates().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let s = new_session(&d, dir.path(), "cufflink").await;
        let other = new_session(&d, dir.path(), "cufflink-docs").await;
        let order = |text: &str, sessions: Vec<uuid::Uuid>| {
            let ids: Vec<String> = sessions.iter().map(ToString::to_string).collect();
            json!({ "text": text, "session_ids": ids })
        };
        let r = call(&crew, super::super::items::ORDER, order("Make sure the cufflink migration keeps going", vec![s.id])).await;
        assert!(r.success, "{:?}", r.error);
        let r = call(&crew, super::super::items::ORDER, order("the cufflink migration: keep it going, please", vec![s.id, other.id])).await;
        assert!(r.success, "{:?}", r.error);
        assert!(r.output.to_string().contains("already kept"), "{}", r.output);
        let orders = || async {
            d.store()
                .items_not_closed(Some("desk"))
                .await
                .unwrap()
                .into_iter()
                .filter(|i| i.kind == OpenKind::Order)
                .collect::<Vec<_>>()
        };
        let one = orders().await;
        assert_eq!(one.len(), 1, "the same order twice is one: {one:?}");
        assert_eq!(one[0].text, "Make sure the cufflink migration keeps going");
        assert!(one[0].sessions.contains(&s.id) && one[0].sessions.contains(&other.id));
        // About other sessions, the same words are another order.
        let third = new_session(&d, dir.path(), "billing").await;
        let r = call(&crew, super::super::items::ORDER, order("the cufflink migration, keep it going please", vec![third.id])).await;
        assert!(r.success, "{:?}", r.error);
        assert!(!r.output.to_string().contains("already kept"), "{}", r.output);
        let mut two = orders().await;
        assert_eq!(two.len(), 2);
        two.retain(|i| i.sessions == vec![third.id]);
        let r = call(&crew, super::super::items::CLOSE, json!({ "item_id": two[0].id.to_string(), "why": "Set aside for the test" })).await;
        assert!(r.success, "{:?}", r.error);
        // Recorded by the boss, it is its note, never the person's words.
        let words = crew.orders_words("desk", None).await.unwrap();
        assert!(
            words.contains("Orders the boss noted") && !words.contains("in their own words"),
            "{words}"
        );
        // Orders that differ stay apart.
        for text in ["Don't start on any new work", "Leave the export alone"] {
            let r = call(&crew, super::super::items::ORDER, order(text, vec![])).await;
            assert!(r.success, "{:?}", r.error);
        }
        let all = orders().await;
        assert_eq!(all.len(), 3);
        let id = |starts: &str| {
            all.iter()
                .find(|i| i.text.starts_with(starts))
                .unwrap()
                .id
                .to_string()
        };
        let close = |item: String, input: Value| {
            let crew = crew.clone();
            async move {
                let mut input = input;
                input["item_id"] = json!(item);
                call(&crew, super::super::items::CLOSE, input).await
            }
        };
        let r = close(id("Don't start"), json!({ "why": "Duplicate of the no-new-work order" })).await;
        assert!(r.success, "{:?}", r.error);
        let r = close(id("Make sure"), json!({ "why": "The person is back in the morning and has had the overnight report" })).await;
        assert!(r.success, "{:?}", r.error);
        // What was done is short, or asked again.
        let r = close(id("Leave the export"), json!({ "done": "Left the export alone all night, as you asked, and checked it twice before morning" })).await;
        assert!(!r.success, "a long done is asked again");
        let r = close(id("Leave the export"), json!({ "done": "Export left untouched all night" })).await;
        assert!(r.success, "{:?}", r.error);

        let update = eventually("the update", async || {
            overseer
                .sessions
                .find(DESK_CHANNEL, PERSON)
                .unwrap()?
                .messages
                .into_iter()
                .find(|m| m.content.contains("✓ Export left untouched all night"))
                .map(|m| m.content)
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let dm: Vec<String> = overseer
            .sessions
            .find(DESK_CHANNEL, PERSON)
            .unwrap()
            .unwrap()
            .messages
            .into_iter()
            .map(|m| m.content)
            .collect();
        for waffle in [
            "Duplicate of",
            "The person is back",
            "The change you asked for",
            "Leave the export alone",
            "cufflink migration keeps going",
        ] {
            assert!(
                dm.iter().all(|m| !m.contains(waffle)),
                "{waffle} said: {dm:?}\n{update}"
            );
        }
    })
    .await;
}

/// With the composer off, an order closed with what was done is said at
/// once, as its one "✓" line.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_composer_what_was_done_is_said_at_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!("{ACT}updates = {{ enabled = false }}\n");
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", &toml, &[]).await;
        let r = call(
            &crew,
            super::super::items::ORDER,
            json!({ "text": "Leave the export alone tonight" }),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let id = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.kind == OpenKind::Order)
            .unwrap()
            .id;
        let r = call(
            &crew,
            super::super::items::CLOSE,
            json!({ "item_id": id.to_string(), "done": "Export left untouched all night" }),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        eventually("said at once", async || {
            said(&overseer, "✓ Export left untouched all night")
        })
        .await;
    })
    .await;
}

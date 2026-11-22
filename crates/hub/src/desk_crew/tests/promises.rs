//! The boss's promises and the person's orders (overseer H6): kept with the
//! sessions they are about until those sessions' accounts show them done,
//! a status question's promise kept by the session's reply, an order from
//! a DM turn that acted on sessions, and each lapsing when its sessions end
//! or nothing changes for 3 days.

use super::super::actions::{Action, Target, STATUS_QUESTION};
use super::updates::begun;
use super::*;
use workforce_desk::open_item::{ClosedBy, NewItem, OpenKind, OpenState};

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
            restated: false,
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
            restated: false,
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
            said(&overseer, "I asked acme-web at ")
        })
        .await;
        assert!(
            report.contains("“Which API key did you use?”, and it says: The staging key"),
            "{report}"
        );
    })
    .await;
}

/// Asking a session where it is again keeps the one promise, which now
/// reports on the newest question: a reply to that one keeps it.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_status_question_moves_its_promise_onto_the_newest_question() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        let ask = |q: &'static str| {
            let crew = crew.clone();
            async move {
                crew.perform(
                    &Action::Nudge {
                        to: Target::Session(s.id),
                        message: format!("{STATUS_QUESTION} {q}"),
                        reply_to: None,
                    },
                    crew.by_overseer("The person asked.").unwrap(),
                )
                .await
                .unwrap();
            }
        };
        let promise = || async {
            d.store()
                .items_not_closed(Some("desk"))
                .await
                .unwrap()
                .into_iter()
                .filter(|i| i.kind == OpenKind::Promise)
                .collect::<Vec<_>>()
        };
        ask("Which API key did you use?").await;
        let first = promise().await.pop().expect("the promise");
        ask("Is the migration done?").await;
        let promises = promise().await;
        assert_eq!(promises.len(), 1, "one promise per session");
        let again = &promises[0];
        assert_eq!(again.id, first.id);
        assert!(
            again.text.contains("Is the migration done?"),
            "{}",
            again.text
        );
        let newest = again.line_message.expect("the question it reports on");
        assert_ne!(Some(newest), first.line_message, "the newest question");
        let mut reply = workforce_desk::line::LineMessage::new(
            workforce_desk::crew::Party::session(&d.view(s.id).unwrap()),
            workforce_desk::crew::Party {
                kind: PartyKind::Manager,
                id: "desk".into(),
                name: "Scout".into(),
                engine: None,
                origin: None,
            },
            workforce_desk::line::LineKind::Answer,
            "Yes, all 20 tables moved.".into(),
            0,
        );
        reply.in_reply_to = Some(newest);
        crew.replied(&reply).await;
        let closed = eventually("kept by the reply to the newest question", async || {
            d.store()
                .open_item(first.id)
                .await
                .unwrap()
                .filter(|i| i.state == OpenState::Closed)
        })
        .await;
        assert_eq!(closed.closed_by, Some(ClosedBy::Boss));
    })
    .await;
}

/// The Cufflink case (16 Nov): a session's reply opened on the mark it
/// answered ("Re m:9d966bcb:") and the boss said only that line. A reply
/// is said as what was asked and what it answered, in words, with its
/// session's link; one that only acknowledges, or holds only its mark, is
/// not posted; no mark ever reaches the person.
#[tokio::test(flavor = "multi_thread")]
async fn a_sessions_reply_is_said_in_words_never_its_mark() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let s = new_session(&d, dir.path(), "Cufflink Migration").await;
        let ask = async |q: &str| {
            crew.perform(
                &Action::Nudge {
                    to: Target::Session(s.id),
                    message: format!("{STATUS_QUESTION} {q}"),
                    reply_to: None,
                },
                crew.by_overseer("The person asked.").unwrap(),
            )
            .await
            .unwrap();
            d.store()
                .items_not_closed(Some("desk"))
                .await
                .unwrap()
                .into_iter()
                .find(|i| i.kind == OpenKind::Promise && i.text.contains(q))
                .expect("the promise")
        };
        let reply = async |text: &str, promise: uuid::Uuid| {
            d.line_call(s.id, "tell", &json!({"to": "desk", "text": text}), None)
                .await
                .unwrap();
            eventually("kept by the reply", async || {
                d.store()
                    .open_item(promise)
                    .await
                    .unwrap()
                    .filter(|i| i.state == OpenState::Closed)
            })
            .await
        };
        let dm = || {
            overseer
                .sessions
                .find(DESK_CHANNEL, PERSON)
                .unwrap()
                .map(|s| s.messages)
                .unwrap_or_default()
        };

        // Only its mark, then only an acknowledgement: nothing is posted.
        let p = ask("Did you start?").await;
        let closed = reply("Re m:9d966bcb:", p.id).await;
        assert!(closed.closed_why.unwrap().starts_with("Cufflink Migration acknowledged"));
        let p = ask("Is the export paused?").await;
        reply("Re m:9d966bcb:\nDone as told.", p.id).await;

        // An answer: what was asked and what it said, with its link.
        let p = ask("How many tables are left?").await;
        reply(
            "Re m:9d966bcb:\nSure. 18 of 20 tables are moved; the last 2 wait on a lock.",
            p.id,
        )
        .await;
        let post = eventually("the answer said", async || {
            dm().into_iter().find(|m| m.content.starts_with("I asked Cufflink Migration"))
        })
        .await;
        assert!(
            post.content.contains(
                "“How many tables are left?”, and it says: 18 of 20 tables are moved; the last 2 wait on a lock."
            ),
            "{}",
            post.content
        );
        let extras = d.store().extras(std::slice::from_ref(&post.id)).await.unwrap();
        let said = &extras[&post.id].update.as_ref().expect("its item").said;
        assert_eq!(said.len(), 1);
        assert_eq!(
            said[0].session.as_ref().map(|l| l.session_id.clone()),
            Some(s.id.to_string())
        );
        // To a question about the work, "Done." is its answer: said.
        let p = ask("Is the migration finished?").await;
        reply("Re m:9d966bcb: Done.", p.id).await;
        eventually("the done said", async || {
            dm().into_iter().find(|m| {
                m.content.contains("“Is the migration finished?”, and it says: Done.")
            })
        })
        .await;
        // Nothing else was said of these replies, and no mark anywhere.
        let all = dm();
        assert!(
            all.iter().all(|m| !m.content.contains("m:9d966bcb")
                && !m.content.contains("Re m:")
                && !m.content.trim().is_empty()),
            "{:?}",
            all.iter().map(|m| &m.content).collect::<Vec<_>>()
        );
        // An update of its own nudges, once their gather (300 ms here) is
        // past on a slow run, is no word of these replies.
        let update = Some(workforce_common::channels::desk::DeliveryKind::Update);
        assert_eq!(
            all.iter()
                .filter(|m| m.desk.as_ref().and_then(|d| d.delivery) != update)
                .filter(|m| m.content.contains("Cufflink Migration"))
                .count(),
            2,
            "{:?}",
            all.iter().map(|m| &m.content).collect::<Vec<_>>()
        );
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
        begun(&crew).await;
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
        // "Ship it" about two sessions is two orders.
        for to in [s.id, other.id] {
            let r = call(&crew, super::super::items::ORDER, order("Ship it", vec![to])).await;
            assert!(r.success, "{:?}", r.error);
            assert!(!r.output.to_string().contains("already kept"), "{}", r.output);
        }
        let ships: Vec<_> = orders().await.into_iter().filter(|i| i.text == "Ship it").collect();
        assert_eq!(ships.len(), 2, "{ships:?}");
        for i in ships {
            let r = call(&crew, super::super::items::CLOSE, json!({ "item_id": i.id.to_string(), "why": "Set aside for the test" })).await;
            assert!(r.success, "{:?}", r.error);
        }
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

/// What the boss owes the person is chased, never left to wait on the
/// session (20 Nov: the partner-docs differences, owed from about 03:00,
/// brought only at 06:51 when the person asked): past its due time the boss
/// is woken once to get it itself from the session's transcript and files,
/// and once as long again, the person is told it is still owed. Each once.
#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_promise_is_chased_by_the_boss_then_told_to_the_person() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let s = new_session(&d, dir.path(), "Legacy removal").await;
        crew.perform(
            &Action::Nudge {
                to: Target::Session(s.id),
                message: format!("{STATUS_QUESTION} Which partner docs differ?"),
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
        assert!(promise.due_at.is_some(), "a promise to report back is due");
        // While the question waits to go into its running session, it is
        // chased as a message that didn't reach it, never as owed as well.
        let asked = promise.line_message.expect("the question it reports on");
        let _ = d
            .store()
            .line_set_waiting(asked, workforce_desk::line::Delivery::Queued)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let chased = d.store().latest_check_in("desk").await.unwrap();
        assert!(
            chased.is_none_or(|c| !c.prompt.contains("it is due and not kept")),
            "chased while its question waits"
        );
        assert_eq!(said(&overseer, "Still owed to you: "), None);
        let moved = d.store().open_item(promise.id).await.unwrap().unwrap();
        assert!(
            moved.due_at > promise.due_at && moved.chased_at.is_none(),
            "its due time moves on while its question waits: {:?} after {:?}",
            moved.due_at,
            promise.due_at
        );
        // Held while its session is stopped, nothing else chases it: it is
        // chased as owed.
        d.store()
            .line_set_waiting(asked, workforce_desk::line::Delivery::Held)
            .await
            .unwrap()
            .expect("it waited to go in");
        let woke = eventually("the boss told to get it itself", async || {
            d.store()
                .latest_check_in("desk")
                .await
                .unwrap()
                .filter(|c| c.prompt.contains("it is due and not kept"))
        })
        .await;
        assert!(
            woke.prompt.contains("Which partner docs differ?")
                && woke.prompt.contains("crew.history")
                && woke.prompt.contains(&promise.id.to_string()),
            "{}",
            woke.prompt
        );
        let told = eventually("the person told it is still owed", async || {
            said(&overseer, "Still owed to you: ")
        })
        .await;
        assert!(told.contains("Which partner docs differ?"), "{told}");
        let after = d.store().open_item(promise.id).await.unwrap().unwrap();
        assert_eq!(after.state, OpenState::Open, "still owed until kept");
        assert_eq!(after.due_at, None, "and chased no more");
        // Each once.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let line = overseer
            .sessions
            .find(DESK_CHANNEL, PERSON)
            .unwrap()
            .unwrap();
        let notices = line
            .messages
            .iter()
            .filter(|m| m.content.starts_with("Still owed to you: "))
            .count();
        assert_eq!(notices, 1);
        let latest = d.store().latest_check_in("desk").await.unwrap().unwrap();
        assert!(
            latest.id == woke.id || !latest.prompt.contains("it is due and not kept"),
            "woken again: {}",
            latest.prompt
        );
    })
    .await;
}

/// A promise the boss keeps with a due time is chased like a report back;
/// one with no due time (a milestone, "merged") never is.
#[tokio::test(flavor = "multi_thread")]
async fn a_promise_with_a_due_time_is_chased_and_one_without_never_is() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let s = new_session(&d, dir.path(), "Legacy removal").await;
        let kept = call(
            &crew,
            "crew.promise",
            json!({"session_id": s.id.to_string(), "text": "Bring you the partner-docs differences",
                   "done_when": "brought", "due_minutes": 1}),
        )
        .await;
        assert!(
            kept.output.to_string().contains("due by"),
            "{}",
            kept.output
        );
        call(
            &crew,
            "crew.promise",
            json!({"session_id": s.id.to_string(), "text": "Tell you when it merges",
                   "done_when": "merged"}),
        )
        .await;
        let items = d.store().items_not_closed(Some("desk")).await.unwrap();
        let due = |text: &str| items.iter().find(|i| i.text == text).unwrap().clone();
        let brought = due("Bring you the partner-docs differences");
        assert!(brought.due_at.is_some());
        assert_eq!(due("Tell you when it merges").due_at, None);
        let woke = eventually("the boss told to get it itself", async || {
            d.store()
                .latest_check_in("desk")
                .await
                .unwrap()
                .filter(|c| c.prompt.contains("it is due and not kept"))
        })
        .await;
        assert!(
            woke.prompt
                .contains("“Bring you the partner-docs differences”")
                && woke.prompt.contains(&brought.id.to_string()),
            "{}",
            woke.prompt
        );
        let closed = call(
            &crew,
            "crew.close_item",
            json!({"item_id": brought.id.to_string(), "done": "brought the nine differences"}),
        )
        .await;
        assert!(closed.success, "{:?}", closed.error);
        assert!(
            !woke.prompt.contains("Tell you when it merges"),
            "{}",
            woke.prompt
        );
    })
    .await;
}

/// A boss that doesn't act (Watch) leaves what it owes to the person: told
/// at its due time that it is still owed, once, and never woken for it.
#[tokio::test(flavor = "multi_thread")]
async fn below_act_a_due_promise_is_told_to_the_person_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, _crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"watch\"\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "Legacy removal").await;
        let since = chrono::Utc::now();
        let p = NewItem::promise(
            "desk",
            s.id,
            "Bring you the partner-docs differences",
            "brought",
        )
        .due(Some(since));
        d.store().item_open(&p).await.unwrap();
        let told = eventually("the person told", async || {
            said(&overseer, "Still owed to you: ")
        })
        .await;
        assert!(told.contains("partner-docs differences"), "{told}");
        let item = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.kind == OpenKind::Promise)
            .unwrap();
        assert_eq!((item.due_at, item.chased_at), (None, None));
        assert_eq!(d.store().wake_ups_since("desk", since).await.unwrap(), 0);
    })
    .await;
}

/// A due promise whose session ended is said once, as still owed and no
/// longer tracked: it never lapses before it is chased, nor again after.
#[tokio::test(flavor = "multi_thread")]
async fn a_due_promise_whose_session_ended_is_said_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"watch\"\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "Legacy removal").await;
        let p = NewItem::promise(
            "desk",
            s.id,
            "Bring you the partner-docs differences",
            "brought",
        )
        .due(Some(chrono::Utc::now() + chrono::Duration::hours(1)));
        d.store().item_open(&p).await.unwrap();
        d.forget(s.id).await.unwrap();
        crew.look_at_items().await;
        assert!(
            d.store()
                .items_not_closed(Some("desk"))
                .await
                .unwrap()
                .iter()
                .any(|i| i.kind == OpenKind::Promise),
            "not lapsed before it is due"
        );
        let due = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.kind == OpenKind::Promise)
            .unwrap();
        d.store()
            .item_chased(due.id, due.due_at.unwrap(), None, Some(chrono::Utc::now()))
            .await
            .unwrap();
        let told = eventually("the person told", async || {
            said(&overseer, "Still owed to you: ")
        })
        .await;
        assert!(told.ends_with("so I'm no longer tracking it."), "{told}");
        let closed = d.store().open_item(due.id).await.unwrap().unwrap();
        assert_eq!(closed.closed_by, Some(ClosedBy::Lapsed));
        crew.look_at_items().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(said(&overseer, "I'm no longer tracking"), None, "said once");
    })
    .await;
}

/// At Act, a due promise whose session ended wakes no one: there is no
/// session to read. It is said once, as still owed and no longer tracked.
#[tokio::test(flavor = "multi_thread")]
async fn at_act_a_due_promise_whose_session_ended_is_said_once_and_wakes_no_one() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, _crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let s = new_session(&d, dir.path(), "Legacy removal").await;
        let since = chrono::Utc::now();
        d.forget(s.id).await.unwrap();
        let p = NewItem::promise(
            "desk",
            s.id,
            "Bring you the partner-docs differences",
            "brought",
        )
        .due(Some(since));
        let opened = match d.store().item_open(&p).await.unwrap() {
            workforce_desk::open_item_store::Opened::New(i)
            | workforce_desk::open_item_store::Opened::Again(i) => i,
        };
        let told = eventually("the person told", async || {
            said(&overseer, "Still owed to you: ")
        })
        .await;
        assert!(told.ends_with("so I'm no longer tracking it."), "{told}");
        let closed = d.store().open_item(opened.id).await.unwrap().unwrap();
        assert_eq!(closed.closed_by, Some(ClosedBy::Lapsed));
        assert_eq!(d.store().wake_ups_since("desk", since).await.unwrap(), 0);
    })
    .await;
}

/// A chase moves only the due time it read: never one chased, asked afresh
/// or closed since; asking afresh starts its chase over.
#[tokio::test(flavor = "multi_thread")]
async fn a_chase_moves_only_the_due_time_it_read() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, _crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"watch\"\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "Legacy removal").await;
        let store = d.store();
        // As Postgres keeps it: to the microsecond.
        let later =
            chrono::SubsecRound::trunc_subsecs(chrono::Utc::now() + chrono::Duration::hours(1), 6);
        let promise =
            |at| NewItem::promise("desk", s.id, "Bring you the list", "brought").due(Some(at));
        let opened = match store.item_open(&promise(later)).await.unwrap() {
            workforce_desk::open_item_store::Opened::New(i)
            | workforce_desk::open_item_store::Opened::Again(i) => i,
        };
        let was = opened.due_at.unwrap();
        let next = later + chrono::Duration::minutes(10);
        assert!(store
            .item_chased(opened.id, was, Some(later), Some(next))
            .await
            .unwrap());
        assert!(
            !store
                .item_chased(opened.id, was, Some(later), None)
                .await
                .unwrap(),
            "a stale due time"
        );
        let chased = store.open_item(opened.id).await.unwrap().unwrap();
        assert_eq!(chased.chased_at, Some(later));
        let afresh = later + chrono::Duration::hours(1);
        store.item_open(&promise(afresh)).await.unwrap();
        let asked = store.open_item(opened.id).await.unwrap().unwrap();
        assert_eq!((asked.due_at, asked.chased_at), (Some(afresh), None));
        store
            .item_close(opened.id, ClosedBy::Boss, "Settled.")
            .await
            .unwrap();
        assert!(
            !store
                .item_chased(opened.id, afresh, None, None)
                .await
                .unwrap(),
            "closed"
        );
    })
    .await;
}

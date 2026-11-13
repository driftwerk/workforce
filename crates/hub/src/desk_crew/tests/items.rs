//! What the boss owes the person (overseer H5): one item per matter, kept
//! in Postgres across restarts and DM rollovers; closed by its button in
//! the same request, by the session, or by the boss, always with who and
//! why; said again on its cadence and parked after three; swept nightly;
//! and posted at once, whatever the check-in budget says.

use super::*;
use workforce_desk::crew::{Event, ItemRefused, Kind};
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

fn dm_posts(
    overseer: &AgentService,
    starts: &str,
) -> Vec<workforce_common::session::SessionMessage> {
    overseer
        .sessions
        .find(DESK_CHANNEL, PERSON)
        .unwrap()
        .map(|dm| {
            dm.messages
                .into_iter()
                .filter(|m| m.content.starts_with(starts))
                .collect()
        })
        .unwrap_or_default()
}

/// Asked with `crew.ask_person`, the person gets a needs-you post at once,
/// even with every check-in spent; asked again it is one item and one post;
/// and it is in every turn's block after the DM rolls over.
#[tokio::test(flavor = "multi_thread")]
async fn an_ask_is_one_item_posted_at_once_and_survives_a_rollover() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"suggest\"\nmax_checkins_per_hour = 1\nmax_checkins_per_day = 1\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        // Every check-in spent first.
        let web = session_party("acme-web");
        d.note(
            Event::new(Kind::Question, web.clone(), Some(web), "asked “Ship it?”")
                .detail(json!({"question": {"text": "Ship it?", "options": []}})),
        );
        eventually("a check-in", async || {
            d.store().latest_check_in("desk").await.unwrap()
        })
        .await;
        let (_, o) = crew.overseer_named("desk").unwrap();
        assert!(matches!(
            crew.budget("desk", &o).await.unwrap(),
            super::super::watcher::Budget::Spent { .. }
        ));
        let ask = json!({
            "text": "Merge acme-web's fix with CI red on the flaky e2e job?",
            "session_id": s.id.to_string(),
            "options": ["Merge", "Wait for green"],
            "recommendation": "Wait for green",
            "holding_up": "The billing migration is paused on this",
            "generalizes": "When a session wants to merge with CI red",
        });
        let r = call(&crew, super::super::items::ASK, ask.clone()).await;
        assert!(r.success, "{:?}", r.error);
        let post = eventually("the needs-you post", async || {
            dm_posts(&overseer, "**Needs you.** Merge acme-web").into_iter().next()
        })
        .await;
        assert_eq!(
            post.desk.as_ref().and_then(|m| m.delivery),
            Some(workforce_common::channels::desk::DeliveryKind::Question)
        );
        // What it holds up is behind "more", never in the line.
        assert!(!post.content.contains("Holding up"), "{}", post.content);
        let extras = d.store().extras(std::slice::from_ref(&post.id)).await.unwrap();
        let item = extras[&post.id].item.clone().expect("its buttons");
        assert!(
            item.detail.as_deref().unwrap_or_default().contains("Holding up: The billing migration"),
            "{:?}",
            item.detail
        );
        assert_eq!(item.options, vec!["Merge", "Wait for green"]);
        assert_eq!(item.recommendation.as_deref(), Some("Wait for green"));

        // The same case asked again, in other words: updated, never said
        // twice.
        let mut again = ask.clone();
        again["text"] = json!("Can acme-web merge #14 with CI red?");
        let r = call(&crew, super::super::items::ASK, again.clone()).await;
        assert!(r.output.to_string().contains("already asked"), "{}", r.output);
        // The check-in's mark is the boss's alone, never said to the person.
        let items: Vec<_> = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .filter(|i| i.said())
            .collect();
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].id.to_string(), item.item_id);
        assert_eq!(dm_posts(&overseer, "**Needs you.**").len(), 1);

        // The answer it recommends is one of its options: one that isn't
        // is refused, naming them.
        let mut wrong = again.clone();
        wrong["recommended"] = json!("Ship it");
        let r = call(&crew, super::super::items::ASK, wrong).await;
        assert!(!r.success);
        let said = r.error.clone().unwrap_or_default();
        assert!(said.contains("“Merge”") && said.contains("“Wait for green”"), "{said}");
        // Asked again with no options, it is matched against those it
        // offered; its points and what carries on are kept short.
        let mut laid = again.clone();
        laid.as_object_mut().unwrap().remove("options");
        laid["recommended"] = json!("wait FOR green ");
        // A question of two lines is asked again, as one.
        let mut two = laid.clone();
        two["question"] = json!("Merge with CI red?\nThe e2e job is flaky.");
        let r = call(&crew, super::super::items::ASK, two).await;
        assert!(!r.success, "two lines");
        laid["question"] = json!("Merge with CI red?");
        laid["items"] = json!((0..9).map(|n| format!("Point {n} {}", "x".repeat(200))).collect::<Vec<_>>());
        laid["not_blocking"] = json!("The docs PR carries on.\nSo does the export.");
        let r = call(&crew, super::super::items::ASK, laid).await;
        assert!(r.success, "{:?}", r.error);
        let i = d
            .store()
            .open_item(items[0].id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(i.layout.recommended.as_deref(), Some("Wait for green"));
        assert_eq!(i.layout.question.as_deref(), Some("Merge with CI red?"));
        assert_eq!(i.layout.points.len(), 6);
        assert!(i.layout.points.iter().all(|p| p.chars().count() <= 120));
        assert_eq!(i.layout.not_blocking.as_deref(), Some("The docs PR carries on."));
        let n = workforce_desk::update::NeedsYou::of(&i, chrono::Utc::now(), false);
        assert_eq!(
            n.options.iter().map(|o| o.recommended).collect::<Vec<_>>(),
            vec![false, true]
        );
        // New options without the one recommended drop it.
        let mut moved = again.clone();
        moved["options"] = json!(["Merge now", "Revert the fix"]);
        let r = call(&crew, super::super::items::ASK, moved).await;
        assert!(r.success, "{:?}", r.error);
        let i = d.store().open_item(items[0].id).await.unwrap().unwrap();
        assert_eq!(i.layout.recommended, None);

        // Kept in Postgres, so its DM's rollover loses none of it.
        let mut dm = overseer
            .sessions
            .get_or_create(DESK_CHANNEL, PERSON, "desk")
            .unwrap();
        workforce_agent::rollover::seal_and_mint(overseer.sessions.as_ref(), &mut dm, 0).unwrap();
        let block = super::super::context::block(&overseer, DESK_CHANNEL, PERSON)
            .await
            .expect("a block");
        assert!(block.contains("What you owe the person now"), "{block}");
        assert!(block.contains("merge #14 with CI red"), "{block}");
        let listed = call(&crew, super::super::items::OPEN, json!({})).await;
        assert!(listed.output.to_string().contains("Questions you asked"), "{}", listed.output);
    })
    .await;
}

/// A session's question given to the person twice is one item and one
/// card; their answer from its button closes it in that request.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_given_twice_is_one_card_and_its_button_closes_it() {
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
        let s = new_session(&d, dir.path(), "acme-web").await;
        let asked = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "desk", "text": "May I force push to main?", "wait_seconds": 60}),
                Some(std::time::Duration::from_millis(10)),
            )
            .await
            .unwrap();
        let ask: uuid::Uuid = serde_json::from_value(asked["id"].clone()).unwrap();
        eventually("the card", async || {
            dm_posts(&overseer, "acme-web asks: “May I force push")
                .into_iter()
                .next()
        })
        .await;
        let q = d.store().line_message(ask).await.unwrap().unwrap();
        crew.post_question(&q).await;
        let item = d
            .store()
            .item_of_line(ask)
            .await
            .unwrap()
            .expect("its item");
        assert_eq!(dm_posts(&overseer, "acme-web asks:").len(), 1, "one card");
        assert_eq!(
            d.store()
                .items_not_closed(Some("desk"))
                .await
                .unwrap()
                .len(),
            1
        );

        assert_eq!(crew.answer_item(item.id, "Not yet").await.unwrap(), "Sent.");
        let closed = d.store().open_item(item.id).await.unwrap().unwrap();
        assert_eq!(closed.state, OpenState::Closed, "closed in that request");
        assert_eq!(closed.closed_by, Some(ClosedBy::Person));
        assert_eq!(closed.closed_why.as_deref(), Some("You answered it."));
        let q = d.store().line_message(ask).await.unwrap().unwrap();
        assert_eq!(q.asked, Some(workforce_desk::line::Asked::Answered));
        assert_eq!(
            crew.answer_item(item.id, "Yes").await,
            Err(ItemRefused::Closed)
        );
    })
    .await;
}

/// Every way an item closes says who closed it and why.
#[tokio::test(flavor = "multi_thread")]
async fn each_way_an_item_closes_says_who_and_why() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "Thanks, I'll hold the merge.",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        let store = d.store();
        async fn open_in(
            store: &workforce_desk::store::DeskStore,
            text: &str,
            session: Option<uuid::Uuid>,
        ) -> workforce_desk::open_item::OpenItem {
            let new = NewItem::ask("desk", OpenKind::Ask, session, text);
            store.item_open(&new).await.unwrap().item().clone()
        }
        let open = |text: &'static str, session: Option<uuid::Uuid>| open_in(store, text, session);
        // Its button: closed as the person's, and their answer goes to the
        // boss as theirs.
        let a = open("Ship the docs today?", None).await;
        assert_eq!(
            crew.answer_item(a.id, "Tomorrow").await.unwrap(),
            "Answered."
        );
        let a = store.open_item(a.id).await.unwrap().unwrap();
        assert_eq!(a.closed_by, Some(ClosedBy::Person));
        assert_eq!(a.closed_why.as_deref(), Some("You answered: Tomorrow"));
        eventually("the person's answer in the DM", async || {
            overseer
                .sessions
                .find(DESK_CHANNEL, PERSON)
                .unwrap()?
                .messages
                .into_iter()
                .find(|m| {
                    m.content
                        .contains("My answer to “Ship the docs today?”: Tomorrow")
                })
        })
        .await;
        // Drop.
        let b = open("Rename the repo?", None).await;
        crew.drop_item(b.id).await.unwrap();
        let b = store.open_item(b.id).await.unwrap().unwrap();
        assert_eq!(
            (b.closed_by, b.closed_why.as_deref()),
            (Some(ClosedBy::Person), Some("You dropped it."))
        );
        // The boss, with why; another boss's item isn't its to close.
        let c = open("Bump the Node version?", None).await;
        let r = call(
            &crew,
            super::super::items::CLOSE,
            json!({ "item_id": c.id.to_string(), "why": "A preference settles it." }),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let c = store.open_item(c.id).await.unwrap().unwrap();
        assert_eq!(c.closed_by, Some(ClosedBy::Boss));
        let other = store
            .item_open(&NewItem::ask("scout", OpenKind::Ask, None, "Theirs?"))
            .await
            .unwrap()
            .item()
            .clone();
        let r = call(
            &crew,
            super::super::items::CLOSE,
            json!({ "item_id": other.id.to_string(), "why": "No." }),
        )
        .await;
        assert!(!r.success);
        // The session: gone, and its request answered in its terminal.
        let gone = open("Retry the deploy?", Some(uuid::Uuid::new_v4())).await;
        let mut request = NewItem::ask(
            "desk",
            OpenKind::Ask,
            Some(s.id),
            "acme-web waits on a request",
        );
        request.dedupe_key = workforce_desk::open_item::approval_key(
            &s.id.to_string(),
            &uuid::Uuid::new_v4().to_string(),
        );
        let request = store.item_open(&request).await.unwrap().item().clone();
        crew.look_at_items().await;
        let gone = store.open_item(gone.id).await.unwrap().unwrap();
        assert_eq!(
            (gone.closed_by, gone.closed_why.as_deref()),
            (Some(ClosedBy::Session), Some("Its session is gone."))
        );
        let request = store.open_item(request.id).await.unwrap().unwrap();
        assert_eq!(
            (request.closed_by, request.closed_why.as_deref()),
            (Some(ClosedBy::Session), Some("Its request was answered."))
        );
        // Said to the person, a session's close is said in the DM.
        eventually("the close said", async || {
            dm_posts(&overseer, "No longer waiting on you: Retry the deploy?")
                .into_iter()
                .next()
        })
        .await;
        // A session's question it withdrew closes its item.
        let asked = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "you", "text": "Which region?", "wait_seconds": 60}),
                Some(std::time::Duration::from_millis(10)),
            )
            .await
            .unwrap();
        let ask: uuid::Uuid = serde_json::from_value(asked["id"].clone()).unwrap();
        let q = d.store().line_message(ask).await.unwrap().unwrap();
        crew.post_question(&q).await;
        let item = eventually("its item", async || {
            d.store().item_of_line(ask).await.unwrap()
        })
        .await;
        let mut q = d.store().line_message(ask).await.unwrap().unwrap();
        q.asked = Some(workforce_desk::line::Asked::Withdrawn);
        d.store().put_line(&q).await.unwrap();
        crew.look_at_items().await;
        let item = store.open_item(item.id).await.unwrap().unwrap();
        assert_eq!(
            (item.closed_by, item.closed_why.as_deref()),
            (Some(ClosedBy::Session), Some("The session withdrew it."))
        );
    })
    .await;
}

/// Unanswered, an ask is said again once it is 30 minutes old, then every
/// reminder, and parked by the third restatement.
#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_ask_is_restated_on_its_cadence_and_parked_after_three() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        // A minute is 100 ms: first said again after 3 s, then every 100 ms.
        let (overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\nwake = { remind_minutes = 1 }\n\
             # Its own posts, as without the composer.\n\
             updates = { enabled = false }\n",
            &[],
        )
        .await;
        let mut ids = Vec::new();
        for n in 0..5 {
            let new = NewItem::ask("desk", OpenKind::Ask, None, &format!("Question {n}?"));
            ids.push(d.store().item_open(&new).await.unwrap().item().id);
        }
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(
            dm_posts(&overseer, "Still waiting on you").is_empty(),
            "not before 30 minutes"
        );
        // Said again past 30 minutes (at most three at once and the rest
        // counted, as `items::restatements` is tested to say).
        eventually("the first restatements", async || {
            let p = dm_posts(&overseer, "Still waiting on you");
            (!p.is_empty()).then_some(p)
        })
        .await;
        let parked = eventually("all parked", async || {
            let items = d.store().items_not_closed(Some("desk")).await.unwrap();
            items
                .iter()
                .all(|i| i.state == OpenState::Parked)
                .then_some(items)
        })
        .await;
        assert!(parked.iter().all(|i| i.restated() == 3), "{parked:?}");
        let said = dm_posts(&overseer, "Still waiting on you");
        assert_eq!(said.len(), 15, "three times each, then never");
        assert_eq!(
            said.iter()
                .filter(|m| m.content.contains("I won't bring this up again"))
                .count(),
            5
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(dm_posts(&overseer, "Still waiting on you").len(), 15);
        let listed = call(&crew, super::super::items::OPEN, json!({})).await;
        assert!(
            listed.output.to_string().contains("Parked"),
            "{}",
            listed.output
        );
        // A parked item still takes its answer.
        assert_eq!(crew.answer_item(ids[0], "Yes").await.unwrap(), "Answered.");
    })
    .await;
}

/// The nightly sweep closes what no longer applies, and nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn the_sweep_closes_only_what_no_longer_applies() {
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
        let store = d.store();
        let gone = store
            .item_open(&NewItem::ask(
                "desk",
                OpenKind::Ask,
                Some(uuid::Uuid::new_v4()),
                "Gone?",
            ))
            .await
            .unwrap()
            .item()
            .clone();
        let live = store
            .item_open(&NewItem::ask(
                "desk",
                OpenKind::Ask,
                Some(s.id),
                "Still live?",
            ))
            .await
            .unwrap()
            .item()
            .clone();
        crew.sweep_items(chrono::Duration::zero()).await;
        let gone = store.open_item(gone.id).await.unwrap().unwrap();
        assert_eq!(gone.state, OpenState::Closed);
        assert_eq!(gone.closed_why.as_deref(), Some("Its session is gone."));
        let live = store.open_item(live.id).await.unwrap().unwrap();
        assert_eq!(live.state, OpenState::Open, "it still applies");
    })
    .await;
}

/// What `hub.crew.raised` kept moves to the items once, and the key goes;
/// what the watcher tells the boss is kept there from then on.
#[tokio::test(flavor = "multi_thread")]
async fn the_raised_questions_move_to_the_items_once() {
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
        let key = format!("asked:{}:Which port?", s.id);
        let at = chrono::Utc::now() - chrono::Duration::minutes(5);
        d.store()
            .put_state(
                super::super::watcher::RAISED,
                &json!({ key.clone(): at, "old:x:y": at - chrono::Duration::days(2) }),
            )
            .await
            .unwrap();
        let marks = crew.raised_marks().await;
        assert!(marks.contains_key(&key), "{marks:?}");
        assert!(!marks.contains_key("old:x:y"), "a day old: gone");
        assert!(d
            .store()
            .state(super::super::watcher::RAISED)
            .await
            .unwrap()
            .is_none());
        // The boss's mark is never said to the person.
        let items = d.store().items_not_closed(Some("desk")).await.unwrap();
        assert!(items.iter().all(|i| !i.said()), "{items:?}");
        assert!(crew.items_context("desk").await.is_none());
    })
    .await;
}

/// A DM reply that ends on a question to the person is an ask of its own,
/// unless that turn asked with crew.ask_person.
#[tokio::test(flavor = "multi_thread")]
async fn a_reply_ending_on_a_question_opens_one_unless_the_turn_asked() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "The fix is ready. Shall I merge it now?",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        crew.run_dm_turn(&overseer, PERSON, "How is the fix going?", 0, None, None)
            .await
            .unwrap();
        let item = eventually("its item", async || {
            d.store()
                .items_not_closed(Some("desk"))
                .await
                .unwrap()
                .into_iter()
                .next()
        })
        .await;
        assert_eq!(item.text, "Shall I merge it now?");
        assert!(item.said());
        crew.drop_item(item.id).await.unwrap();
        // Asked with the tool in that turn: the reply is not kept again.
        crew.asked_person.lock().insert(
            "desk".into(),
            chrono::Utc::now() + chrono::Duration::seconds(30),
        );
        crew.run_dm_turn(&overseer, PERSON, "And now?", 0, None, None)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .is_empty());
    })
    .await;
}

/// A parked item waits for the person: no closer and no sweep takes it,
/// even once what it was about is gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_parked_item_is_never_closed_under_the_person() {
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
        let store = d.store();
        let i = store
            .item_open(&NewItem::ask(
                "desk",
                OpenKind::Ask,
                Some(s.id),
                "Merge it?",
            ))
            .await
            .unwrap()
            .item()
            .clone();
        store
            .item_said(i.id, chrono::Utc::now(), true)
            .await
            .unwrap();
        d.forget(s.id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        crew.look_at_items().await;
        crew.sweep_items(chrono::Duration::zero()).await;
        let i = store.open_item(i.id).await.unwrap().unwrap();
        assert_eq!(i.state, OpenState::Parked);
        crew.drop_item(i.id).await.unwrap();
        assert_eq!(
            store.open_item(i.id).await.unwrap().unwrap().state,
            OpenState::Closed
        );
    })
    .await;
}

/// A request left to the person is an item posted with its own buttons,
/// and answering it closes the item as that answer goes.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_left_to_the_person_closes_as_they_answer_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        let url = hook_url(dir.path(), s.id).await;
        let asked = tokio::spawn(async move {
            hook(&url, json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": "git push --force"}})).await
        });
        let pending = eventually("the request", async || d.view(s.id).unwrap().pending).await;
        crew.approval_left_open(s.id, pending.id, "Run git push --force").await;
        let post = eventually("its needs-you post", async || {
            dm_posts(&overseer, "**Needs you.** acme-web waits on your answer")
                .into_iter()
                .next()
        })
        .await;
        let extras = d.store().extras(std::slice::from_ref(&post.id)).await.unwrap();
        assert_eq!(extras[&post.id].approvals[0].approval_id, pending.id.to_string());
        let key = workforce_desk::open_item::approval_key(&s.id.to_string(), &pending.id.to_string());
        let item = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.dedupe_key == key)
            .unwrap();
        d.decide(s.id, pending.id, workforce_desk::model::Decision::Deny, Some(By::you()))
            .unwrap();
        asked.await.unwrap();
        let closed = eventually("closed as answered", async || {
            d.store()
                .open_item(item.id)
                .await
                .unwrap()
                .filter(|i| i.state == OpenState::Closed)
        })
        .await;
        assert_eq!(
            (closed.closed_by, closed.closed_why.as_deref()),
            (Some(ClosedBy::Person), Some("You answered it."))
        );
    })
    .await;
}

/// With close_from_accounts on, an ask closes once its session's account
/// says it chose for itself since and waits on nothing; off, it stays.
#[tokio::test(flavor = "multi_thread")]
async fn an_account_closes_an_ask_only_with_the_switch_on() {
    for on in [false, true] {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let (_overseer, crew, d, _llm) = overseeing(
                dir.path(),
                pool,
                "ok",
                &format!(
                    "[oversight]\nenabled = true\nlevel = \"act\"\nupdates = {{ close_from_accounts = {on} }}\n"
                ),
                &[],
            )
            .await;
            let s = new_session(&d, dir.path(), "acme-web").await;
            let i = d
                .store()
                .item_open(&NewItem::ask("desk", OpenKind::Ask, Some(s.id), "Keep the export change?"))
                .await
                .unwrap()
                .item()
                .clone();
            let mut a = workforce_desk::account::Account::new(s.id, "", chrono::Utc::now());
            a.by_model = true;
            // A choice it made before it was asked settles nothing.
            a.decisions.push(workforce_desk::account::Decision {
                what: "Kept the export as it was".into(),
                why: "Before the ask".into(),
                at: i.opened_at - chrono::Duration::hours(1),
            });
            d.store().session_account_put(&a).await.unwrap();
            crew.look_at_items().await;
            assert_eq!(
                d.store().open_item(i.id).await.unwrap().unwrap().state,
                OpenState::Open,
                "an older choice doesn't close it"
            );
            let mut a = d
                .store()
                .session_account(s.id)
                .await
                .unwrap()
                .unwrap();
            a.decisions.push(workforce_desk::account::Decision {
                what: "Went ahead without the export change".into(),
                why: "The person didn't answer".into(),
                at: chrono::Utc::now(),
            });
            d.store().session_account_put(&a).await.unwrap();
            crew.look_at_items().await;
            let i = d.store().open_item(i.id).await.unwrap().unwrap();
            match on {
                true => {
                    assert_eq!(i.closed_by, Some(ClosedBy::Session));
                    assert!(i.closed_why.unwrap().contains("went ahead"));
                }
                false => assert_eq!(i.state, OpenState::Open),
            }
        })
        .await;
    }
}

/// An answer from its button that doesn't reach the boss is never lost: the
/// item is open again and the person is told.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_doesnt_reach_the_boss_opens_its_item_again() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        let i = d
            .store()
            .item_open(&NewItem::ask(
                "desk",
                OpenKind::Ask,
                None,
                "Ship the docs today?",
            ))
            .await
            .unwrap()
            .item()
            .clone();
        let closed = d
            .store()
            .item_close(i.id, ClosedBy::Person, "You answered: Tomorrow")
            .await
            .unwrap()
            .unwrap();
        crew.answer_lost(&overseer, PERSON, &closed).await;
        let again = d.store().open_item(i.id).await.unwrap().unwrap();
        assert_eq!(again.state, OpenState::Open);
        assert_eq!(again.closed_by, None);
        eventually("the person told", async || {
            dm_posts(
                &overseer,
                "Your answer to “Ship the docs today?” didn't reach",
            )
            .into_iter()
            .next()
        })
        .await;
        // Another of its key opened since: it isn't reopened over it.
        let closed = d
            .store()
            .item_close(i.id, ClosedBy::Person, "You answered: Tomorrow")
            .await
            .unwrap()
            .unwrap();
        d.store()
            .item_open(&NewItem::ask(
                "desk",
                OpenKind::Ask,
                None,
                "Ship the docs today?",
            ))
            .await
            .unwrap();
        assert!(!d.store().item_reopen(closed.id).await.unwrap());
    })
    .await;
}

const FULL: &str = "[oversight]\nenabled = true\nlevel = \"full\"\n\
                    [oversight.full]\nrepos = [\"acme/acme-web\"]\n";

/// The first screenshot's case: at Act fully, what only the person can do
/// (rotate their own key) is never asked with answers or a recommendation.
/// Asked so, it is refused; as a todo it is one "For you." line with no
/// buttons, closed by its Done.
#[tokio::test(flavor = "multi_thread")]
async fn at_act_fully_what_only_the_person_can_do_is_one_line_with_done() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", FULL, &[]).await;
        let s = new_session(&d, dir.path(), "acme-api").await;
        let text = "acme-api's CI log printed the staging deploy key. The key is in your own vault account, which I can't reach, so I can't rotate it myself. My plan once it's rotated: re-run the deploy, then check the bucket's access log for the last day.";
        let ask = json!({
            "text": text,
            "session_id": s.id.to_string(),
            "options": ["Rotate now", "After the release", "Leave it"],
            "recommendation": "Rotate now: the log is readable across the org.",
            "holding_up": "The staging deploy waits on a fresh key",
        });
        let r = call(&crew, super::super::items::ASK, ask.clone()).await;
        assert!(!r.success, "asked with answers at Act fully");
        assert!(
            r.error.as_deref().unwrap_or_default().contains("kind todo"),
            "{:?}",
            r.error
        );
        assert!(dm_posts(&overseer, "**Needs you.**").is_empty());

        let mut todo = ask.clone();
        todo["kind"] = json!("todo");
        todo["question"] = json!("Rotate the staging deploy key in your vault.");
        let r = call(&crew, super::super::items::ASK, todo).await;
        assert!(r.success, "{:?}", r.error);
        let post = eventually("the for-you post", async || {
            dm_posts(&overseer, "**For you.**").into_iter().next()
        })
        .await;
        assert_eq!(
            post.content.lines().next().unwrap_or_default(),
            "**For you.** Rotate the staging deploy key in your vault."
        );
        assert!(!post.content.contains("My plan"), "{}", post.content);
        assert!(!post.content.contains("Rotate now"), "{}", post.content);
        let extras = d.store().extras(std::slice::from_ref(&post.id)).await.unwrap();
        let item = extras[&post.id].item.clone().expect("its card");
        assert!(item.options.is_empty(), "{:?}", item.options);
        assert_eq!(item.recommendation, None);
        assert!(item.for_you);
        let id: uuid::Uuid = item.item_id.parse().unwrap();

        assert_eq!(crew.done_item(id).await.unwrap(), "Done.");
        let closed = d.store().open_item(id).await.unwrap().unwrap();
        assert_eq!(
            (closed.state, closed.closed_by, closed.closed_why.as_deref()),
            (OpenState::Closed, Some(ClosedBy::Person), Some("You did it."))
        );
        assert_eq!(crew.done_item(id).await, Err(ItemRefused::Closed));
    })
    .await;
}

/// The second screenshot's case: a session's question while the person is
/// typing in that session is theirs, and nothing is posted; one posted
/// before they stepped in is settled at the next look, said as `settled`,
/// and reads as settled by them.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_the_person_is_answering_in_the_session_is_left_to_them() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        d.person_typing(s.id).unwrap();
        let asked = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "desk", "text": "Should I drop the staging orders table and re-seed it from the fixtures, or migrate the rows in place? The fixtures are a week old.", "wait_seconds": 60}),
                Some(std::time::Duration::from_millis(10)),
            )
            .await
            .unwrap();
        let ask: uuid::Uuid = serde_json::from_value(asked["id"].clone()).unwrap();
        let q = d.store().line_message(ask).await.unwrap().unwrap();
        crew.post_question(&q).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(dm_posts(&overseer, "acme-web asks:").is_empty(), "nothing posted");
        assert!(d.store().item_of_line(ask).await.unwrap().is_none());

        // Asked of the person before they stepped in: posted, then settled.
        let other = new_session(&d, dir.path(), "acme-api").await;
        let asked = d
            .line_call(
                other.id,
                "ask",
                &json!({"to": "desk", "text": "May I bump the API's rate limit to 200 a minute for the load test?", "wait_seconds": 60}),
                Some(std::time::Duration::from_millis(10)),
            )
            .await
            .unwrap();
        let ask: uuid::Uuid = serde_json::from_value(asked["id"].clone()).unwrap();
        let q = d.store().line_message(ask).await.unwrap().unwrap();
        crew.post_question(&q).await;
        let card = eventually("the card", async || {
            dm_posts(&overseer, "acme-api asks:").into_iter().next()
        })
        .await;
        let item = d.store().item_of_line(ask).await.unwrap().expect("its item");
        let (_, mut events) = d.subscribe();
        tokio::time::sleep(Duration::from_millis(50)).await;
        d.person_typing(other.id).unwrap();
        crew.look_at_updates().await;
        let closed = eventually("settled", async || {
            d.store()
                .open_item(item.id)
                .await
                .unwrap()
                .filter(|i| i.state == OpenState::Closed)
        })
        .await;
        assert_eq!(
            closed.closed_why.as_deref(),
            Some(workforce_desk::open_item::SETTLED_WHY)
        );
        let settled = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(workforce_desk::model::DeskEvent::Activity { event }) = events.recv().await {
                    if event.kind == Kind::Settled {
                        return event;
                    }
                }
            }
        })
        .await
        .expect("the settled event");
        let detail = settled.detail.clone().unwrap_or_default();
        assert_eq!(detail["itemId"], json!(item.id.to_string()));
        assert_eq!(detail["by"], json!("you"));
        assert_eq!(
            workforce_desk::open_item::settled_by(&closed),
            Some(workforce_desk::update::SettledBy::You)
        );
        assert!(!card.content.is_empty());
    })
    .await;
}

//! The typing hold, presence and the catch-up (overseer H8): updates wait
//! for a pause and at most the cap, and for a reply being written;
//! needs-you waits only for typing, and still posts while the person is
//! away; away is judged from the DM's read point, with or without
//! presence pings; one catch-up per return.

use super::*;
use workforce_common::channels::desk::DeliveryKind;
use workforce_desk::account::{Account, Done, Ref, RefKind};
use workforce_desk::open_item::ClosedBy;

/// The DM's messages delivered as `kind`, with when they were posted.
fn delivered(
    overseer: &AgentService,
    kind: DeliveryKind,
) -> Vec<workforce_common::session::SessionMessage> {
    overseer
        .sessions
        .find(DESK_CHANNEL, PERSON)
        .unwrap()
        .map(|s| s.messages)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.desk.as_ref().and_then(|d| d.delivery) == Some(kind))
        .collect()
}

/// Adds `done` (a PR ref when it names one) to `s`'s account.
async fn did(d: &Desk, s: uuid::Uuid, done: &str) {
    let mut a = d
        .session_accounts_of(&[s])
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| Account::new(s, "", chrono::Utc::now()));
    a.purpose = "moving the lead picker".into();
    a.done.push(Done {
        text: done.into(),
        refs: done
            .contains("PR")
            .then(|| Ref {
                kind: RefKind::Pr,
                id: "#622".into(),
                url: None,
            })
            .into_iter()
            .collect(),
        at: chrono::Utc::now(),
    });
    a.next = "Deploy it".into();
    d.store().session_account_put(&a).await.unwrap();
}

async fn begun(crew: &Arc<CrewHub>) {
    crew.look_at_updates().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
}

/// Updates after a minute's gather (100 ms here), away after 5 (500 ms).
const ACT: &str = "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n\
                   updates = { every_minutes = 1, max_wait_minutes = 1, away_after_minutes = 5 }\n";

/// Pings typing in the boss's DM every 20 ms for `for_ms`.
fn type_for(crew: &Arc<CrewHub>, for_ms: u64) -> tokio::task::JoinHandle<()> {
    let crew = Arc::clone(crew);
    tokio::spawn(async move {
        let until = std::time::Instant::now() + Duration::from_millis(for_ms);
        while std::time::Instant::now() < until {
            assert!(crew.typing_in("desk"));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
}

/// While the person types the update waits; typing on, it goes at the cap
/// (three minutes: 300 ms here) all the same; a pause lets it go sooner.
#[tokio::test(flavor = "multi_thread")]
async fn an_update_waits_for_a_pause_and_never_past_the_cap() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        assert!(!crew.typing_in("nobody"), "no boss by that name");
        begun(&crew).await;
        let s = new_session(&d, dir.path(), "acme-web").await;

        // Typing on for a second: held, then out at the cap.
        let started = std::time::Instant::now();
        let typing = type_for(&crew, 1000);
        did(&d, s.id, "Opened PR #622").await;
        let posted = eventually("the update", async || {
            delivered(&overseer, DeliveryKind::Update)
                .into_iter()
                .next()
        })
        .await;
        let at = started.elapsed();
        assert!(
            at >= Duration::from_millis(280),
            "held while typing: {at:?}"
        );
        assert!(
            !typing.is_finished(),
            "out at the cap while still typing: {at:?}"
        );
        assert!(posted.content.contains("Opened PR #622"));
        typing.await.unwrap();

        // A short burst: out once they pause, before the cap.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = chrono::Utc::now();
        let typing = type_for(&crew, 60);
        did(&d, s.id, "Merged PR #622").await;
        let second = eventually("the second update", async || {
            delivered(&overseer, DeliveryKind::Update)
                .into_iter()
                .nth(1)
        })
        .await;
        assert!(typing.is_finished(), "held while typing");
        // Posted at the pause, before the cap (300 ms from when the typing
        // began) would have let it go: when it was posted, not when the
        // test saw it.
        let at = second.timestamp - started;
        assert!(
            at < chrono::Duration::milliseconds(280),
            "a pause lets it go before the cap: {at:?}"
        );
    })
    .await;
}

/// An update due while the boss is writing a reply in the DM goes after
/// it, never in the middle of it.
#[tokio::test(flavor = "multi_thread")]
async fn an_update_waits_for_a_running_reply() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        begun(&crew).await;
        let sid = overseer
            .sessions
            .find(DESK_CHANNEL, PERSON)
            .unwrap()
            .unwrap()
            .id
            .to_string();
        crew.manager_turns
            .lock()
            .insert(sid.clone(), chrono::Utc::now());
        let s = new_session(&d, dir.path(), "acme-web").await;
        did(&d, s.id, "Opened PR #622").await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(
            delivered(&overseer, DeliveryKind::Update).is_empty(),
            "not in the middle of a reply"
        );
        // The reply ended.
        crew.manager_turns.lock().remove(&sid);
        crew.update_poke.notify_one();
        eventually("the update", async || {
            delivered(&overseer, DeliveryKind::Update)
                .into_iter()
                .next()
        })
        .await;
    })
    .await;
}

/// A needs-you waits for the person's pause too, but no longer than the
/// cap, and without holding up the turn that asked.
#[tokio::test(flavor = "multi_thread")]
async fn a_needs_you_waits_only_for_typing_and_the_cap() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, _d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let started = std::time::Instant::now();
        let typing = type_for(&crew, 1000);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let r = ask(&crew, "Which key should staging use?").await;
        assert!(r.success, "{:?}", r.error);
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "the asking turn isn't held up"
        );
        assert!(needs_you(&overseer).is_empty(), "held while typing");
        eventually("the needs-you", async || {
            needs_you(&overseer).into_iter().next()
        })
        .await;
        let at = started.elapsed();
        assert!(at >= Duration::from_millis(280), "{at:?}");
        assert!(
            !typing.is_finished(),
            "out at the cap while still typing: {at:?}"
        );
        typing.await.unwrap();
    })
    .await;
}

/// A needs-you settled while it waits on the person's typing never posts,
/// and one a restart left held goes once its time is past, once.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_needs_you_goes_only_while_open_and_survives_a_restart() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let typing = type_for(&crew, 150);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let r = ask(&crew, "Which key should staging use?").await;
        assert!(r.success, "{:?}", r.error);
        let store = d.store();
        let item = store
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .remove(0);
        // Answered another way while it waited.
        store
            .item_close(item.id, ClosedBy::Person, "Staging.")
            .await
            .unwrap();
        typing.await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(needs_you(&overseer).is_empty(), "settled: no card");
        assert!(store
            .cards_due(chrono::Utc::now())
            .await
            .unwrap()
            .is_empty());

        // A card held when the hub went down, its item still open.
        let r = ask(&crew, "Which region for the demo?").await;
        assert!(r.success, "{:?}", r.error);
        let open = store
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .remove(0);
        eventually("its card", async || needs_you(&overseer).into_iter().next()).await;
        let held = workforce_desk::update_store::HeldCard {
            id: uuid::Uuid::new_v4(),
            boss: "desk".into(),
            item_id: Some(open.id),
            text: "**Needs you.** Which region for the demo?".into(),
            extras: workforce_desk::crew::Extras {
                item: Some(open.mention()),
                ..Default::default()
            },
            post_by: chrono::Utc::now() - chrono::Duration::seconds(1),
        };
        store.card_hold(&held).await.unwrap();
        crew.post_due_cards().await;
        crew.post_due_cards().await;
        assert_eq!(
            needs_you(&overseer).len(),
            2,
            "posted once after the restart"
        );
        assert!(store
            .cards_due(chrono::Utc::now())
            .await
            .unwrap()
            .is_empty());
    })
    .await;
}

/// An update that didn't go before the person went away is not sent on
/// its own: their return is one catch-up that covers it.
#[tokio::test(flavor = "multi_thread")]
async fn an_update_left_unsent_is_folded_into_the_catch_up() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        crew.test_reads
            .lock()
            .insert("desk".into(), chrono::Utc::now());
        begun(&crew).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        did(&d, s.id, "Opened PR #622").await;
        eventually("the update", async || {
            delivered(&overseer, DeliveryKind::Update)
                .into_iter()
                .next()
        })
        .await;
        eventually("away", async || {
            d.store()
                .update_state("desk", chrono::Utc::now())
                .await
                .unwrap()
                .away_since
        })
        .await;
        // An update written but not sent (its send failed, say).
        did(&d, s.id, "Merged PR #622").await;
        let p = workforce_desk::update::Pending {
            id: uuid::Uuid::new_v4(),
            text: "Merged PR #622.".into(),
            update: Default::default(),
            told: Default::default(),
            restated: Vec::new(),
            cutoff: Some(chrono::Utc::now()),
            handed: false,
            sent: None,
            away: None,
        };
        d.store().update_pend("desk", &p).await.unwrap();
        crew.update_poke.notify_one();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            delivered(&overseer, DeliveryKind::Update).len(),
            1,
            "not sent alone"
        );
        crew.present(true, Some("desk"));
        let back = eventually("the catch-up", async || {
            delivered(&overseer, DeliveryKind::Away).into_iter().next()
        })
        .await;
        assert!(back.content.contains("Merged PR #622"), "{}", back.content);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(delivered(&overseer, DeliveryKind::Update).len(), 1);
        assert_eq!(delivered(&overseer, DeliveryKind::Away).len(), 1);
    })
    .await;
}

async fn ask(crew: &Arc<CrewHub>, text: &str) -> workforce_agent::tools::ToolResult {
    let tool = super::super::tools::CrewTool::for_level(crew, "desk", &Default::default(), false)
        .into_iter()
        .find(|t| workforce_agent::tools::Tool::name(t) == super::super::items::ASK)
        .unwrap();
    workforce_agent::tools::Tool::execute(
        &tool,
        json!({ "text": text, "options": ["Staging", "None"], "recommendation": "Staging" }),
    )
    .await
    .unwrap()
}

fn needs_you(overseer: &AgentService) -> Vec<workforce_common::session::SessionMessage> {
    delivered(overseer, DeliveryKind::Question)
        .into_iter()
        .filter(|m| m.content.starts_with("**Needs you.**"))
        .collect()
}

/// Busy in session terminals with Desk attended, the person is away once
/// the DM's posts go unread: updates gather, needs-you still posts, and
/// one catch-up covers it all when the DM is on screen again.
#[tokio::test(flavor = "multi_thread")]
async fn away_while_busy_elsewhere_and_one_catch_up_on_return() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        crew.test_reads
            .lock()
            .insert("desk".into(), chrono::Utc::now());
        crew.present(true, None);
        begun(&crew).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        did(&d, s.id, "Opened PR #622").await;
        let first = eventually("the update", async || {
            delivered(&overseer, DeliveryKind::Update)
                .into_iter()
                .next()
        })
        .await;
        // In the terminals, Desk attended, the DM unread: away.
        let since = eventually("away", async || {
            crew.present(true, None);
            d.store()
                .update_state("desk", chrono::Utc::now())
                .await
                .unwrap()
                .away_since
        })
        .await;
        // Postgres keeps microseconds.
        assert_eq!(
            since,
            chrono::SubsecRound::trunc_subsecs(first.timestamp, 6)
        );

        // What changes now gathers; a needs-you still goes out.
        did(&d, s.id, "Merged PR #622").await;
        let r = ask(&crew, "Which key should staging use?").await;
        assert!(r.success, "{:?}", r.error);
        eventually("the needs-you", async || {
            needs_you(&overseer).into_iter().next()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            delivered(&overseer, DeliveryKind::Update).len(),
            1,
            "gathered"
        );
        assert!(delivered(&overseer, DeliveryKind::Away).is_empty());

        // The DM on screen again: one catch-up.
        crew.present(true, Some("desk"));
        let back = eventually("the catch-up", async || {
            delivered(&overseer, DeliveryKind::Away).into_iter().next()
        })
        .await;
        assert!(
            back.content
                .starts_with("Welcome back. One thing needs you."),
            "{}",
            back.content
        );
        assert!(back.content.contains("Merged PR #622"), "{}", back.content);
        // The update that began the absence went unread: covered too.
        assert!(back.content.contains("Opened PR #622"), "{}", back.content);
        assert!(
            back.content
                .contains("Still waiting on you:\n- Which key should staging use?"),
            "{}",
            back.content
        );
        // Its extras are kept just after it posts.
        let extras = eventually("its extras", async || {
            d.store()
                .extras(std::slice::from_ref(&back.id))
                .await
                .unwrap()
                .remove(&back.id)
                .filter(|e| e.away.is_some())
        })
        .await;
        let away = extras.away.expect("the catch-up's extras");
        assert_eq!(away.since, since);
        let update = extras.update.expect("what it covers");
        assert_eq!(update.sessions[0].session_id, s.id.to_string());
        assert_eq!(update.waiting[0].options, vec!["Staging", "None"]);
        // Desk read it: no second catch-up, and updates go on as usual.
        crew.test_reads
            .lock()
            .insert("desk".into(), chrono::Utc::now());
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(delivered(&overseer, DeliveryKind::Away).len(), 1, "once");
        assert_eq!(delivered(&overseer, DeliveryKind::Update).len(), 1);
        let state = d
            .store()
            .update_state("desk", chrono::Utc::now())
            .await
            .unwrap();
        assert!(state.away_since.is_none() && state.pending.is_none());
    })
    .await;
}

/// With no presence pings (Desk closed, or an older one), the read point
/// alone says the person is away, and its moving says they are back. A
/// hub whose sessions keep no read points never counts them away.
#[tokio::test(flavor = "multi_thread")]
async fn away_without_pings_from_the_read_point_alone() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        crew.test_reads
            .lock()
            .insert("desk".into(), chrono::Utc::now());
        begun(&crew).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        did(&d, s.id, "Opened PR #622").await;
        eventually("the update", async || {
            delivered(&overseer, DeliveryKind::Update)
                .into_iter()
                .next()
        })
        .await;
        eventually("away", async || {
            d.store()
                .update_state("desk", chrono::Utc::now())
                .await
                .unwrap()
                .away_since
        })
        .await;
        did(&d, s.id, "Merged PR #622").await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(delivered(&overseer, DeliveryKind::Update).len(), 1);
        // They read the DM.
        crew.test_reads
            .lock()
            .insert("desk".into(), chrono::Utc::now());
        let back = eventually("the catch-up", async || {
            delivered(&overseer, DeliveryKind::Away).into_iter().next()
        })
        .await;
        assert!(back.content.starts_with("Welcome back. Nothing needs you."));

        // No read points at all: never away, however long unread.
        crew.test_reads.lock().clear();
        did(&d, s.id, "Deployed PR #622").await;
        eventually("an update", async || {
            (delivered(&overseer, DeliveryKind::Update).len() == 2).then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(d
            .store()
            .update_state("desk", chrono::Utc::now())
            .await
            .unwrap()
            .away_since
            .is_none());
    })
    .await;
}

/// At Watch and Suggest a check-in is the update: while the person is away
/// it gathers for their return, rather than posting into the DM.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_in_that_only_tells_gathers_while_away() {
    use workforce_desk::crew::{Event, Kind};
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "acme-web finished.",
            "[oversight]\nenabled = true\nlevel = \"suggest\"\n\
             wake = { waiting_minutes = 0, quiet_minutes = 0 }\n\
             updates = { every_minutes = 1, max_wait_minutes = 1, away_after_minutes = 5 }\n",
            &[],
        )
        .await;
        crew.test_reads
            .lock()
            .insert("desk".into(), chrono::Utc::now());
        crew.present(true, None);
        let store = d.store();
        let s = session_party("acme-web");
        d.note(Event::new(
            Kind::Finished,
            s.clone(),
            Some(s),
            "finished · all green",
        ));
        let first = eventually("the check-in", async || {
            store.latest_check_in("desk").await.unwrap()
        })
        .await
        .id;
        // Its post unread, in the terminals: away.
        eventually("away", async || {
            crew.present(true, None);
            store
                .update_state("desk", chrono::Utc::now())
                .await
                .unwrap()
                .away_since
        })
        .await;
        let s = session_party("workforce-e7");
        d.note(Event::new(
            Kind::Finished,
            s.clone(),
            Some(s),
            "finished · merged",
        ));
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(
            store.latest_check_in("desk").await.unwrap().map(|c| c.id),
            Some(first.clone()),
            "gathered while away"
        );
        // Back: it goes.
        crew.present(true, Some("desk"));
        let next = eventually("the gathered check-in", async || {
            store
                .latest_check_in("desk")
                .await
                .unwrap()
                .filter(|c| c.id != first)
        })
        .await;
        assert!(next.prompt.contains("workforce-e7"), "{}", next.prompt);
    })
    .await;
}

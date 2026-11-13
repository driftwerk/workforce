//! The typing hold, presence and the catch-up (overseer H8): updates wait
//! for a pause and at most the cap, and for a reply being written;
//! needs-you waits only for typing, and still posts while the person is
//! away; away is judged from the DM's read point, with or without
//! presence pings; one catch-up per return.

use super::*;
use workforce_common::channels::desk::DeliveryKind;
use workforce_desk::account::{Account, Done, Ref, RefKind};

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
        let started = std::time::Instant::now();
        let typing = type_for(&crew, 120);
        did(&d, s.id, "Merged PR #622").await;
        eventually("the second update", async || {
            (delivered(&overseer, DeliveryKind::Update).len() == 2).then_some(())
        })
        .await;
        let at = started.elapsed();
        assert!(typing.is_finished(), "held while typing: {at:?}");
        assert!(
            at < Duration::from_millis(300),
            "a pause lets it go: {at:?}"
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
        assert!(
            back.content
                .contains("Still waiting on you:\n- Which key should staging use?"),
            "{}",
            back.content
        );
        let extras = d
            .store()
            .extras(std::slice::from_ref(&back.id))
            .await
            .unwrap()
            .remove(&back.id)
            .unwrap();
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

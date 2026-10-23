//! Session-lifecycle integration tests over a REAL persistent store.
//!
//! Exercises the exact production flow the hub's turn paths compose —
//! resolve (`get_or_create`) → stale-resume rollover → turn append →
//! archive slice → scroll-back (`read_thread_page`) — against
//! `JsonlSessionStore`, so regressions in the interplay (not just the
//! units) fail here instead of shipping. This is the harness the iOS
//! stale-replay bug (May session replayed verbatim in July) would have
//! failed loudly in.

use chrono::{DateTime, Utc};
use workforce_agent::rollover::{self, CARRYOVER_PREFIX};
use workforce_common::session::{CompactionConfig, JsonlSessionStore, Session, SessionStore};

fn at(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339)
        .unwrap()
        .with_timezone(&Utc)
}

/// Mimic one hub turn: append user+assistant messages, save, and archive
/// exactly the slice the turn added (serve.rs slices from the pre-turn
/// length, which is what keeps carry-overs live-only and out of archives).
fn run_turn(store: &JsonlSessionStore, session: &mut Session, user: &str, assistant: &str) {
    let archive_base = session.messages.len();
    session.add_user_message(user, None);
    session.add_assistant_message(assistant, 5);
    store.save(session).expect("save turn");
    for msg in &session.messages[archive_base..] {
        store
            .append_to_archive(&session.id, msg)
            .expect("archive turn slice");
    }
}

fn backdate(session: &mut Session, to: DateTime<Utc>) {
    for m in &mut session.messages {
        m.timestamp = to;
    }
}

#[test]
fn dormant_conversation_resumes_dated_and_scrollback_stays_clean() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = JsonlSessionStore::new(dir.path()).expect("store");
    let cfg = CompactionConfig::default();

    // May: a real conversation happens on the persistent iOS thread.
    let mut may = store.get_or_create("ios", "kings", "allan").unwrap();
    may.metadata = serde_json::json!({ "voice_mode": true, "tts_provider": "fishaudio" });
    run_turn(
        &store,
        &mut may,
        "let's bump the pulumi digest",
        "bumping it now",
    );
    run_turn(&store, &mut may, "thanks, also check the CI", "CI is green");
    backdate(&mut may, at("2026-05-25T18:00:00Z"));
    store.save(&may).unwrap();
    let may_id = may.id.clone();

    // July: resume after 47 dormant days — exactly the prod incident shape.
    let now = at("2026-07-11T18:31:00Z");
    let resumed = store.get_or_create("ios", "kings", "allan").unwrap();
    assert_eq!(resumed.id, may_id, "same thread resolves before the roll");
    let mut fresh = rollover::roll_if_stale(&store, resumed, &cfg, now).unwrap();

    assert_ne!(fresh.id, may_id, "dormant session must not be resumed live");
    assert!(store.get(&may_id).unwrap().unwrap().is_sealed());
    assert!(fresh.is_voice(), "voice mode survives the roll");
    let carry = &fresh.messages[0].content;
    assert!(carry.starts_with(CARRYOVER_PREFIX));
    assert!(carry.contains("ended on 2026-05-25"), "{carry}");
    assert!(carry.contains("pulumi digest"), "{carry}");

    // Both resolution seams (gateway anchor + turn path) hitting the fresh
    // successor in the same instant must not mint further sessions.
    let again = rollover::roll_if_stale(&store, fresh.clone(), &cfg, now).unwrap();
    assert_eq!(again.id, fresh.id, "no session accretion on double-resolve");

    // The July turn runs on the successor and archives its slice only.
    run_turn(&store, &mut fresh, "hey what's up", "hey! all quiet today");

    // Scroll-back walks the whole chain chronologically and NEVER shows the
    // carry-over (live-only, pre-archive_base, never archived).
    let page = store
        .read_thread_page(&fresh.id, None, None, 100)
        .expect("thread page");
    let contents: Vec<&str> = page.messages.iter().map(|e| e.content.as_str()).collect();
    assert!(
        contents.iter().all(|c| !c.contains(CARRYOVER_PREFIX)),
        "carry-over leaked into user-facing scroll-back: {contents:?}"
    );
    let joined = contents.join("\n");
    let may_pos = joined.find("pulumi digest").expect("May turn present");
    let july_pos = joined.find("all quiet today").expect("July turn present");
    assert!(may_pos < july_pos, "chain reads chronologically");

    // The thread now routes every resolution to the successor.
    let active = store.get_or_create("ios", "kings", "allan").unwrap();
    assert_eq!(active.id, fresh.id);
}

#[test]
fn active_conversation_is_never_rolled_and_voice_ceiling_still_applies() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = JsonlSessionStore::new(dir.path()).expect("store");
    let cfg = CompactionConfig::default();

    let mut session = store.get_or_create("ios", "kings", "allan").unwrap();
    session.metadata = serde_json::json!({ "voice_mode": true });
    run_turn(&store, &mut session, "morning", "good morning!");

    // Same-day resume: passthrough, same session.
    let resumed = rollover::roll_if_stale(
        &store,
        session.clone(),
        &cfg,
        session.messages[0].timestamp + chrono::Duration::hours(2),
    )
    .unwrap();
    assert_eq!(resumed.id, session.id);

    // A voice thread that stays active accrues messages until the count
    // ceiling — the trigger the token watermark can never reach for voice.
    for i in 0..40 {
        run_turn(&store, &mut session, &format!("q{i}"), &format!("a{i}"));
    }
    assert!(
        rollover::count_pressure(&session, &cfg),
        "82 conversational messages must exceed the default voice ceiling of 80"
    );
}

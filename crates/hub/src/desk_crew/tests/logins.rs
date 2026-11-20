//! The person's logins as the boss sees them, and Desk's own moves between
//! them said in one line in the main boss's DM.

use super::decides_at_once::{said, THEIRS};
use super::*;
use workforce_desk::crew::{Event, Party};

#[tokio::test(flavor = "multi_thread")]
async fn desks_move_between_logins_is_said_once_in_the_bosss_dm() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, _crew, d, _llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let moved = "Kingsley Apple hit its weekly limit at 18:53; I moved auth-fix and invites to Kingsley Gmail.";
        d.note(Event::new(Kind::LoginMoved, Party::desk(), None, moved));
        eventually("the move said in the DM", async || {
            (said(&overseer, &[moved]) == 1).then_some(())
        })
        .await;
        let full = "No Claude login has room to move your sessions to: Kingsley Gmail's weekly limit resets Fri 16:00.";
        d.note(Event::new(Kind::LoginsFull, Party::desk(), None, full));
        eventually("that none has room, said", async || {
            (said(&overseer, &[full]) == 1).then_some(())
        })
        .await;
        // Desk's other activity isn't the DM's.
        d.note(Event::new(Kind::Restarted, Party::desk(), None, "restarted"));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(said(&overseer, &[moved]), 1, "said once");
        assert_eq!(said(&overseer, &["restarted"]), 0);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_login_the_boss_names_that_isnt_there_is_said_with_the_ones_there_are() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        let seen = workforce_desk::scope::Seen::new(
            workforce_common::oversight::Scope::Ungrouped,
            Default::default(),
            Default::default(),
        );
        let out = crew.logins_seen(&seen);
        assert!(
            out["moves"].as_str().unwrap().contains("by itself"),
            "{out}"
        );
        // The boss names one the person hasn't: told so by the tool, nothing
        // moved.
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.switch_login").then_some(())
        })
        .await;
        let s = new_session(&d, &base, "auth-fix").await;
        let r = overseer
            .tools
            .get("crew.switch_login")
            .unwrap()
            .execute(json!({"session_id": s.id.to_string(), "login": "Kingsley Gmail", "reason": "Its login is full."}))
            .await
            .unwrap();
        assert!(!r.success, "{r:?}");
        assert!(
            r.error
                .as_deref()
                .unwrap_or_default()
                .contains("there are no Claude Code logins"),
            "{r:?}"
        );
    })
    .await;
}

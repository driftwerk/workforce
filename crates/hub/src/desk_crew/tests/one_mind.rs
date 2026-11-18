//! One mind per boss (designs/boss-orders-and-serial-turns.md §2),
//! replayed from 18 Nov, anonymised.
//!
//! - **19:39–19:41, release 3.** Two of the boss's own deciding stretches
//!   ran at once, each on the picture as it began, and each told a
//!   different session to run the same release. Here: two sessions report
//!   "release 3 is ready" on their lines within the gather window; the
//!   boss's first line turn is slow and tells one of them to run it. The
//!   second turn must not begin until the first has ended, and must decide
//!   on a picture that already has the first's instruction.

use std::time::Duration;

use super::one_picture::{asked_with, slow_once, told, Answer, ACT};
use super::*;

const READY_A: &str = "Release 3 is cut and tested on the payments side: ready to run.";
const READY_B: &str = "Release 3 notes are written on the web side; ready to run release 3.";
const RUN: &str = "Run release 3 to production now, and tell me when it is out.";

#[tokio::test(flavor = "multi_thread")]
async fn two_sessions_reports_are_decided_one_after_the_other_on_one_picture() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let a = new_session(&d, dir.path(), "payments").await;
        let b = new_session(&d, dir.path(), "web").await;
        // The first turn, on A's report, takes a while and tells A to run
        // the release.
        slow_once(
            &llm,
            "cut and tested on the payments side",
            Duration::from_millis(2000),
            Answer::Tool(
                "crew_nudge",
                json!({"session_id": a.id.to_string(), "message": RUN, "reason": "It is ready."}),
            ),
        )
        .await;
        d.line_call(a.id, "tell", &json!({"to": "desk", "text": READY_A}), None)
            .await
            .unwrap();
        eventually("A's line turn deciding", async || {
            (!asked_with(&llm, "cut and tested on the payments side")
                .await
                .is_empty())
            .then_some(())
        })
        .await;
        assert!(
            crew.mind_holder("desk")
                .is_some_and(|h| h.what == "a session's line"),
            "the line turn holds its boss's mind"
        );
        // B reports while A's turn decides: it gathers, and no turn on it
        // begins while the boss's mind is held.
        d.line_call(b.id, "tell", &json!({"to": "desk", "text": READY_B}), None)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            asked_with(&llm, "notes are written on the web side")
                .await
                .is_empty(),
            "B's turn began while A's still decided"
        );
        // A is told; then B's turn decides, with A's instruction in front
        // of it.
        eventually("A told to run it", async || {
            told(&d, a.id).await.iter().any(|t| t == RUN).then_some(())
        })
        .await;
        let b_turn = eventually("B's line turn", async || {
            asked_with(&llm, "notes are written on the web side")
                .await
                .into_iter()
                .next()
        })
        .await;
        assert!(
            b_turn.contains("Run release 3 to production now"),
            "B's turn decides on a picture with A's instruction: {b_turn}"
        );
        assert!(
            told(&d, b.id).await.is_empty(),
            "{:?}",
            told(&d, b.id).await
        );
    })
    .await;
}

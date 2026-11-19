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
            // Longer than a line's gather: B comes due while A decides.
            Duration::from_millis(7000),
            Answer::Tool(
                "crew_nudge",
                json!({"session_id": a.id.to_string(), "message": RUN, "reason": "It is ready.",
                       "job": {"kind": "release", "target": "acme/monorepo", "key": "release-3"},
                       "irreversible": true}),
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
        tokio::time::sleep(super::super::line::GATHER + Duration::from_millis(1500)).await;
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

/// The person's own turn in the boss's DM is counted while it runs (crew
/// work yields to it) and never waits for the boss's mind.
#[tokio::test(flavor = "multi_thread")]
async fn the_persons_dm_turn_is_counted_while_it_runs_and_never_waits() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, _d, llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        // The DM's answer, slow: its turn runs a while.
        slow_once(
            &llm,
            "How is release 3 going",
            Duration::from_millis(1500),
            Answer::Tool("crew_overview", json!({})),
        )
        .await;
        // The boss's mind is held by something else the whole time.
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let c = Arc::clone(&crew);
        let holder = tokio::spawn(async move {
            c.with_mind("desk", "a check-in", async move {
                go_rx.await.ok();
            })
            .await
        });
        eventually("the mind held", async || {
            crew.mind_holder("desk").map(|_| ())
        })
        .await;
        // The person's own words in the boss's DM, as Desk sends them.
        let (c, o) = (Arc::clone(&crew), Arc::clone(&overseer));
        let turn = tokio::spawn(async move {
            c.run_dm_turn(
                &o,
                PERSON,
                "How is release 3 going?",
                0,
                None,
                Some(workforce_desk::Speaker::you()),
            )
            .await
        });
        eventually("the person's turn counted", async || {
            crew.person_turn_running("desk").then_some(())
        })
        .await;
        assert!(!crew.mind_free("desk"));
        // It reached the model with the mind still held: it never waited.
        eventually("the person's turn deciding", async || {
            (!asked_with(&llm, "How is release 3 going").await.is_empty()).then_some(())
        })
        .await;
        assert!(crew.mind_holder("desk").is_some());
        turn.await.unwrap().unwrap();
        eventually("no longer counted", async || {
            (!crew.person_turn_running("desk")).then_some(())
        })
        .await;
        go_tx.send(()).unwrap();
        holder.await.unwrap();
        assert!(crew.mind_free("desk"));
    })
    .await;
}

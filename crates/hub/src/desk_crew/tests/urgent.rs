//! A boss's urgent orders as the crew gives them (design note §4):
//! `crew.nudge` with `urgent` reaches a working session with its next tool
//! call's result and says so; one that can't take words mid-turn, or past
//! the limit, gets it at its turn's end and the boss is told; an
//! escalating stand-down that isn't read in time has its turn interrupted
//! through the boss's own `crew.interrupt` gate, and a plain urgent order
//! never does.

use super::*;
use workforce_desk::line::{Delivery, Send, Target, Urgency, When};
use workforce_desk::model::SessionState;

const LETS: &str =
    "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.act]\nnudge = true\ninterrupt = true\n";
const NO_INTERRUPT: &str =
    "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.act]\nnudge = true\ninterrupt = false\n";

/// A session at work on a tool its hooks report, and its hook URL.
async fn working(d: &Desk, base: &Path, name: &str) -> (uuid::Uuid, String) {
    let s = new_session(d, base, name).await;
    let url = hook_url(base, s.id).await;
    hook(
        &url,
        json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_input": {"command": "gh run watch 4411"}}),
    )
    .await;
    eventually("it works", async || {
        (d.view(s.id).unwrap().state == SessionState::Working).then_some(())
    })
    .await;
    (s.id, url)
}

fn done() -> Value {
    json!({"hook_event_name": "PostToolUse", "tool_name": "Bash",
        "tool_input": {"command": "gh run watch 4411"}})
}

async fn nudge(overseer: &AgentService, s: uuid::Uuid, message: &str, urgent: bool) -> Value {
    let out = overseer
        .tools
        .get("crew.nudge")
        .unwrap()
        .execute(json!({"session_id": s.to_string(), "message": message,
            "urgent": urgent, "reason": "It is about to ship the wrong release."}))
        .await
        .unwrap();
    out.output
}

#[tokio::test(flavor = "multi_thread")]
async fn an_urgent_nudge_is_read_with_the_next_tool_call_and_the_boss_is_told_how() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, _crew, d, _llm) = overseeing(&base, pool, "ok", LETS, &[]).await;
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.nudge").then_some(())
        })
        .await;
        let schema = super::super::tools::schema("crew.nudge", Vec::new);
        assert!(
            schema["properties"]["urgent"]["description"]
                .as_str()
                .is_some_and(|d| d.contains("mid-turn")),
            "{schema}"
        );
        let (s, url) = working(&d, &base, "release-prep").await;
        let out = nudge(
            &overseer,
            s,
            "Don't dispatch release 4: it moved to another session.",
            true,
        )
        .await;
        assert_eq!(
            out["done"], "told release-prep; it reads it with its next tool call's result",
            "{out}"
        );
        let said = hook(&url, done()).await;
        let text = said["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or_default();
        assert!(text.contains("Don't dispatch release 4"), "{said}");
        let rows = d.line_thread(s, None, None).await.unwrap();
        let row = rows.iter().find(|m| m.text.contains("release 4")).unwrap();
        assert_eq!(row.delivery, Delivery::Delivered);
        assert_eq!(row.urgency, Some(Urgency::Order));

        // Without urgent, as before: at its turn's end.
        let out = nudge(&overseer, s, "Then write the release notes.", false).await;
        assert_eq!(
            out["done"], "told release-prep; it gets it once its turn ends",
            "{out}"
        );
        assert_eq!(hook(&url, done()).await, json!({}));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_urgent_nudge_a_session_cant_take_mid_turn_or_past_the_limit_waits_and_says_so() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, _crew, d, _llm) = overseeing(&base, pool, "ok", LETS, &[]).await;
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.nudge").then_some(())
        })
        .await;
        // Waiting on a dialog.
        let (s, url) = working(&d, &base, "user-flip").await;
        hook(
            &url,
            json!({"hook_event_name": "Notification", "message": "Claude needs your permission"}),
        )
        .await;
        let out = nudge(&overseer, s, "Stop: don't start release 4.", true).await;
        assert_eq!(
            out["done"],
            "told user-flip; it gets it once its turn ends: it can't take words mid-turn",
            "{out}"
        );

        // Three urgent orders in ten minutes; the fourth queues.
        let (t, _) = working(&d, &base, "second-batch").await;
        for i in 0..3 {
            let out = nudge(&overseer, t, &format!("Correction {i}: use the staging DB."), true).await;
            assert!(out["done"].as_str().unwrap().contains("next tool call"), "{i}: {out}");
        }
        let out = nudge(&overseer, t, "Correction 3: use the staging DB.", true).await;
        assert_eq!(
            out["done"],
            "told second-batch; it gets it once its turn ends: it was sent 3 urgent orders in the last 10 minutes, so this one queues",
            "{out}"
        );
    })
    .await;
}

/// The stand-down #420's assignments send, through Desk's entry point.
async fn stand_down(d: &Desk, crew: &CrewHub, s: uuid::Uuid, urgency: Urgency) -> uuid::Uuid {
    let by = crew
        .by_overseer("Release 4 moved to another session.")
        .unwrap();
    let (m, _) = d
        .line_send_with(
            workforce_desk::LineSender::Overseer(by),
            Send::Tell {
                to: Target::Session(s),
                text: "Stand down from a:3f2a (release 4): don't start it.".into(),
                when: When::Queue,
            },
            Some(urgency),
        )
        .await
        .unwrap();
    m.id
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stand_down_not_read_in_time_is_an_interrupt_through_the_bosss_gate() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", LETS, &[]).await;
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.interrupt").then_some(())
        })
        .await;
        d.set_steer_wait(Duration::from_millis(300));

        // A plain urgent order is never escalated.
        let (plain, _) = working(&d, &base, "docs").await;
        nudge(&overseer, plain, "Use the staging DB.", true).await;
        // Its tool runs on past the wait: interrupted, and the stand-down
        // goes in at the idle after.
        let (s, _) = working(&d, &base, "release-prep").await;
        let row = stand_down(&d, &crew, s, Urgency::Escalate).await;
        eventually("its turn interrupted", async || {
            (d.view(s).unwrap().state == SessionState::Done).then_some(())
        })
        .await;
        let interrupted = d
            .activity(Filter::Manager, None, 20)
            .await
            .unwrap()
            .events
            .into_iter()
            .find(|e| e.kind == Kind::Interrupted)
            .expect("the boss's interrupt");
        assert_eq!(interrupted.actor.id, "desk");
        assert!(
            interrupted
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("stand-down")),
            "{interrupted:?}"
        );
        assert_eq!(
            d.view(plain).unwrap().state,
            SessionState::Working,
            "never escalated"
        );
        let m = d.store().line_message(row).await.unwrap().unwrap();
        assert_ne!(m.delivery, Delivery::Refused);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_boss_not_let_to_interrupt_has_the_interrupt_suggested_and_the_turn_runs_on() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", NO_INTERRUPT, &[]).await;
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.nudge").then_some(())
        })
        .await;
        d.set_steer_wait(Duration::from_millis(300));
        let (s, url) = working(&d, &base, "release-prep").await;
        stand_down(&d, &crew, s, Urgency::Escalate).await;
        let suggested = eventually("the interrupt, suggested", async || {
            d.store()
                .suggestions(10)
                .await
                .unwrap()
                .into_iter()
                .find(|g| g.text == "Interrupt release-prep")
        })
        .await;
        assert_eq!(suggested.state, SuggestionState::Open);
        assert_eq!(d.view(s).unwrap().state, SessionState::Working);
        // Its next call still reads it.
        let said = hook(&url, done()).await;
        assert!(
            said["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .is_some_and(|t| t.contains("Stand down from a:3f2a")),
            "{said}"
        );
    })
    .await;
}

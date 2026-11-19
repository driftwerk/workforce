//! A boss's urgent orders as the crew gives them (design note §4):
//! `crew.nudge` with `urgent` reaches a working session with its next tool
//! call's result and says so; one that can't take words mid-turn, or past
//! the limit, gets it at its turn's end and the boss is told; an
//! escalating stand-down that isn't read in time has its turn interrupted
//! through the boss's own `crew.interrupt` gate, and a plain urgent order
//! never does.

use super::super::actions::{Action, Target as ActionTarget};
use super::super::assignments::{Declared, Order};
use super::*;
use workforce_desk::assignment::short;
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
        let said = schema["properties"]["urgent"]["description"]
            .as_str()
            .unwrap_or_default();
        for words in [
            "mid-turn",
            "gh run watch",
            "crew.interrupt",
            "At most 3 urgent orders per session in 10 minutes",
            "Ignored with reply_to",
        ] {
            assert!(said.contains(words), "{words}: {said}");
        }
        assert!(!said.contains("a stop"), "{said}");
        // Only where it is steered in.
        for tool in [
            "crew.resume",
            "crew.restart",
            "crew.start_session",
            "crew.assign_ticket",
        ] {
            let schema = super::super::tools::schema(tool, Vec::new);
            assert!(
                schema["properties"].get("urgent").is_none(),
                "{tool}: {schema}"
            );
        }
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
        // The stand-down goes in at the idle after.
        went_in(&d, row).await;
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
        // Its next call still reads it, and the suggestion is withdrawn:
        // the person's yes would interrupt what it does next.
        let said = hook(&url, done()).await;
        assert!(
            said["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .is_some_and(|t| t.contains("Stand down from a:3f2a")),
            "{said}"
        );
        eventually("the suggestion withdrawn", async || {
            let now = d.store().suggestion(&suggested.id).await.unwrap()?;
            (now.state == SuggestionState::Dismissed).then_some(())
        })
        .await;
    })
    .await;
}

/// The line row `row` goes in at its session's idle (typed in: no plugin
/// waits on these sessions' lines).
async fn went_in(d: &Desk, row: uuid::Uuid) {
    eventually("in at the idle", async || {
        let m = d.store().line_message(row).await.unwrap()?;
        matches!(m.delivery, Delivery::Delivered | Delivery::Handed).then_some(())
    })
    .await;
}

/// The boss's urgent order holding Harbor's release `key`, a job that
/// can't be undone.
fn release(key: &str) -> Order {
    Order {
        job: Declared::Job {
            kind: "release".into(),
            target: Some("harbor/monorepo".into()),
            key: key.into(),
        },
        irreversible: true,
        urgent: true,
        ..Default::default()
    }
}

fn nudge_action(to: uuid::Uuid, message: &str) -> Action {
    Action::Nudge {
        to: ActionTarget::Session(to),
        message: message.into(),
        reply_to: None,
    }
}

async fn held(d: &Desk, s: uuid::Uuid) -> workforce_desk::assignment::Assignment {
    d.store()
        .assignments_of("desk", true)
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.session == s)
        .expect("an assignment of its")
}

/// Release `key` is ordered to `old` (working, its hook at `url`), urgently,
/// and its next call reads it; moving it to `new` holds `new`'s order and
/// sends `old` its stand-down: that row.
async fn moved(
    crew: &Arc<CrewHub>,
    d: &Desk,
    key: &str,
    (old, url): (uuid::Uuid, &str),
    new: uuid::Uuid,
) -> uuid::Uuid {
    let by = || crew.by_overseer("Sam moved the release.").unwrap();
    let said = crew
        .perform_with(
            &nudge_action(old, &format!("You own release {key}: dispatch it.")),
            by(),
            &release(key),
        )
        .await
        .unwrap();
    assert!(said.contains("next tool call"), "{said}");
    let took = hook(url, done()).await;
    assert!(
        took["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .is_some_and(|t| t.contains(&format!("release {key}"))),
        "{took}"
    );
    // The hook's take is when the order went in.
    let went = eventually("its order went in", async || {
        let a = held(d, old).await;
        a.delivered_at.map(|_| a)
    })
    .await;
    // The session is on its next tool call.
    hook(
        url,
        json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_input": {"command": "gh run watch 4412"}}),
    )
    .await;
    let moving = Order {
        replaces: Some(short(went.id)),
        ..Default::default()
    };
    let said = crew
        .perform_with(
            &nudge_action(new, &format!("Start release {key}.")),
            by(),
            &moving,
        )
        .await
        .unwrap();
    assert!(said.starts_with("held a:"), "{said}");
    held(d, new).await.stand_down.expect("the old holder told")
}

#[tokio::test(flavor = "multi_thread")]
async fn moving_an_irreversible_job_off_a_busy_session_steers_its_stand_down_then_interrupts() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", LETS, &[]).await;
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.interrupt").then_some(())
        })
        .await;
        d.picture_load().await;
        d.set_steer_wait(Duration::from_millis(400));

        // Read in time: its next call carries it; nothing is interrupted.
        let (old, url) = working(&d, &base, "harbor-release-3").await;
        let new = new_session(&d, &base, "harbor-auth-reset").await;
        let row = moved(&crew, &d, "4", (old, &url), new.id).await;
        let m = d.store().line_message(row).await.unwrap().unwrap();
        assert_eq!(
            (m.delivery, m.urgency),
            (Delivery::Queued, Some(Urgency::Escalate))
        );
        let said = hook(&url, done()).await;
        assert!(
            said["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .is_some_and(|t| t.contains("Stand down from")),
            "{said}"
        );
        let m = d.store().line_message(row).await.unwrap().unwrap();
        assert_eq!(m.delivery, Delivery::Delivered);
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(d.view(old).unwrap().state, SessionState::Working);

        // Unread, its tool running on: interrupted, then in at the idle.
        let (busy, url) = working(&d, &base, "harbor-release-5").await;
        let other = new_session(&d, &base, "harbor-docs").await;
        let row = moved(&crew, &d, "5", (busy, &url), other.id).await;
        eventually("its turn interrupted", async || {
            (d.view(busy).unwrap().state == SessionState::Done).then_some(())
        })
        .await;
        went_in(&d, row).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relayed_urgent_nudge_keeps_its_urgency_as_the_machine_performs_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (_overseer, crew, d, _llm) = overseeing(&base, pool, "ok", LETS, &[]).await;
        let (s, url) = working(&d, &base, "acme-api").await;
        // As the home relays it (its job fields stay home), and as the
        // machine's served path performs it: its order read from the input,
        // as a boss (a manager's words; the grant's checks are relay.rs's).
        let input = super::super::assignments::relayed_input(
            &json!({"session_id": s.to_string(), "message": "Don't dispatch release 4.",
                    "urgent": true, "job": {"kind": "release", "key": "4"},
                    "reason": "It moved to another session."}),
            None,
        );
        let mut order = Order::of(&input);
        order.unrecorded = true;
        let by = crew.by_overseer("It moved.").unwrap();
        let said = crew
            .perform_with(&nudge_action(s, "Don't dispatch release 4."), by, &order)
            .await
            .unwrap();
        assert_eq!(
            said, "told acme-api; it reads it with its next tool call's result",
            "{said}"
        );
        let row = d
            .line_thread(s, None, None)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.text.contains("release 4"))
            .expect("on its line");
        assert_eq!(row.urgency, Some(Urgency::Order));
        let took = hook(&url, done()).await;
        assert!(
            took["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .is_some_and(|t| t.contains("Don't dispatch release 4")),
            "{took}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_suggested_interrupt_said_yes_to_after_its_turn_ended_interrupts_nothing() {
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
        let row = stand_down(&d, &crew, s, Urgency::Escalate).await;
        let suggested = eventually("the interrupt, suggested", async || {
            d.store()
                .suggestions(10)
                .await
                .unwrap()
                .into_iter()
                .find(|g| g.text == "Interrupt release-prep")
        })
        .await;
        assert_eq!(
            suggested.action["input"]["stand_down"]["row"],
            json!(row.to_string()),
            "bound to its stand-down"
        );
        // Its turn ends, and the next begins on a long tool.
        hook(
            &url,
            json!({"hook_event_name": "Stop", "last_assistant_message": "Done."}),
        )
        .await;
        hook(
            &url,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "now the docs"}),
        )
        .await;
        hook(
            &url,
            json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
                "tool_input": {"command": "cargo build"}}),
        )
        .await;
        eventually("working again", async || {
            (d.view(s).unwrap().state == SessionState::Working).then_some(())
        })
        .await;
        // The person's yes, late: nothing is interrupted.
        let accepted = workforce_desk::crew::Crew::accept(&*crew, &suggested).await;
        assert!(accepted.is_err(), "{accepted:?}");
        assert_eq!(d.view(s).unwrap().state, SessionState::Working);
        // And it was withdrawn for them, once the turn ended.
        eventually("the suggestion withdrawn", async || {
            let now = d.store().suggestion(&suggested.id).await.unwrap()?;
            (now.state == SuggestionState::Dismissed).then_some(())
        })
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_suggested_interrupt_said_yes_to_in_its_turn_interrupts_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", NO_INTERRUPT, &[]).await;
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.nudge").then_some(())
        })
        .await;
        d.set_steer_wait(Duration::from_millis(300));
        let (s, _) = working(&d, &base, "release-prep").await;
        let row = stand_down(&d, &crew, s, Urgency::Escalate).await;
        let suggested = eventually("the interrupt, suggested", async || {
            d.store()
                .suggestions(10)
                .await
                .unwrap()
                .into_iter()
                .find(|g| g.text == "Interrupt release-prep")
        })
        .await;
        assert_eq!(
            super::super::urgent::only_turn_of(&suggested.action["input"]).map(|(r, _)| r),
            Some(row),
            "it reads back its stand-down"
        );
        // The person's yes while the turn still runs: interrupted.
        workforce_desk::crew::Crew::accept(&*crew, &suggested)
            .await
            .unwrap();
        eventually("its turn interrupted", async || {
            (d.view(s).unwrap().state == SessionState::Done).then_some(())
        })
        .await;
        went_in(&d, row).await;
    })
    .await;
}

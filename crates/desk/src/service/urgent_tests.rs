//! A boss's urgent words to a working session (design note §4): read
//! beside its next tool call's result, delivered once whichever way they
//! go in, at the idle when nothing took them, never past the limit, an
//! escalating stand-down interrupting a turn that doesn't read it, and
//! steered again after a restart.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::line::Sender;
use super::tests::{desk, new_session, post};
use super::{Desk, Rules};
use crate::crew::{By, Crew, CrewAgent, Overseer, Suggestion};
use crate::line::{self, Delivery, LineMessage, Rushed, Send, Target, Unsteerable, Urgency, When};
use crate::model::SessionState;
use crate::store::build_pool;

fn boss() -> By {
    By::manager("sarah", "Sarah", "It must hear this now.")
}

fn tell(to: Uuid, text: &str) -> Send {
    Send::Tell {
        to: Target::Session(to),
        text: text.into(),
        when: When::Queue,
    }
}

fn tool(command: &str) -> Value {
    json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": command}})
}

fn done() -> Value {
    json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": {"command": "ls"}})
}

fn prompt() -> Value {
    json!({"hook_event_name": "UserPromptSubmit", "prompt": "ship the release"})
}

fn stop() -> Value {
    json!({"hook_event_name": "Stop", "last_assistant_message": "Done."})
}

fn context(said: &Value) -> Option<&str> {
    said.pointer("/hookSpecificOutput/additionalContext")
        .and_then(Value::as_str)
}

async fn row(d: &Desk, id: Uuid) -> LineMessage {
    d.0.store.line_message(id).await.unwrap().unwrap()
}

/// A Claude Code session at work on a tool call, its hooks heard, Desk's
/// plugin waiting on its line.
async fn working(d: &Desk, dir: &std::path::Path) -> Uuid {
    let s = new_session(d, dir).await;
    d.plugin_heard_from(s.id);
    post(d, s.id, prompt()).await;
    post(d, s.id, tool("gh run watch")).await;
    assert_eq!(d.view(s.id).unwrap().state, SessionState::Working);
    s.id
}

async fn urgent(d: &Desk, to: Uuid, text: &str, u: Urgency) -> (LineMessage, Rushed) {
    let (m, rushed) = d
        .line_send_with(Sender::Overseer(boss()), tell(to, text), Some(u))
        .await
        .unwrap();
    (m, rushed.unwrap())
}

#[tokio::test]
async fn the_next_tool_call_reads_an_urgent_order_and_its_row_is_delivered_once() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = working(&d, dir.path()).await;
        let (m, rushed) = urgent(
            &d,
            s,
            "Stand down: don't dispatch release 4.",
            Urgency::Order,
        )
        .await;
        assert_eq!(rushed, Rushed::Steered);
        assert_eq!(
            rushed.said().as_deref(),
            Some("it reads it with its next tool call's result")
        );
        let kept = row(&d, m.id).await;
        assert_eq!(
            (kept.delivery, kept.urgency),
            (Delivery::Queued, Some(Urgency::Order))
        );
        // Desk's plugin, waiting on the line mid-turn, is never handed it:
        // the hook takes it.
        assert!(d.line_next(s, Duration::from_millis(200)).await.is_none());
        assert_eq!(row(&d, m.id).await.delivery, Delivery::Queued);
        let said = post(&d, s, done()).await;
        let text = context(&said).expect("it goes with the call's result");
        assert!(text.contains("don't dispatch release 4"), "{text}");
        assert!(
            text.contains(&line::marker(m.id)),
            "marked, to be answered: {text}"
        );
        assert_eq!(said["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        let delivered = row(&d, m.id).await;
        assert_eq!(delivered.delivery, Delivery::Delivered);
        assert!(delivered.delivered_at.is_some());
        // Once: the next call carries nothing, and the turn's end hands
        // nothing again.
        post(&d, s, tool("ls")).await;
        assert_eq!(post(&d, s, done()).await, json!({}));
        post(&d, s, stop()).await;
        assert!(d.line_next(s, Duration::from_millis(200)).await.is_none());
    })
    .await;
}

#[tokio::test]
async fn an_urgent_order_no_call_took_goes_in_at_the_idle_exactly_once() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = working(&d, dir.path()).await;
        let (m, _) = urgent(&d, s, "Use the staging DB.", Urgency::Order).await;
        // Its tool runs to the turn's end: nothing takes it.
        post(&d, s, stop()).await;
        let handed = d
            .line_next(s, Duration::from_secs(5))
            .await
            .expect("the idle takes it");
        assert_eq!(handed["id"], json!(m.id));
        assert_eq!(row(&d, m.id).await.delivery, Delivery::Handed);
        d.line_ack(s, m.id, true).await.unwrap();
        assert_eq!(row(&d, m.id).await.delivery, Delivery::Delivered);
        // The next turn's first call doesn't carry it a second time.
        post(&d, s, prompt()).await;
        post(&d, s, tool("ls")).await;
        assert_eq!(post(&d, s, done()).await, json!({}));
    })
    .await;
}

#[tokio::test]
async fn a_call_that_finds_its_row_gone_in_another_way_says_nothing_of_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = working(&d, dir.path()).await;
        let (m, _) = urgent(&d, s, "Use the staging DB.", Urgency::Order).await;
        // Its inbox took it first (a session may read it mid-turn).
        let inbox = d.line_call(s, "inbox", &json!({}), None).await.unwrap();
        assert!(inbox.to_string().contains("staging DB"), "{inbox}");
        assert_eq!(row(&d, m.id).await.delivery, Delivery::Delivered);
        assert_eq!(post(&d, s, done()).await, json!({}), "never twice");
    })
    .await;
}

#[tokio::test]
async fn a_sub_agents_call_leaves_the_bosss_order_for_the_session() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = working(&d, dir.path()).await;
        let (m, _) = urgent(&d, s, "Stop the deploy.", Urgency::Order).await;
        let mut theirs = done();
        theirs["agent_id"] = json!("a471");
        theirs["agent_type"] = json!("general-purpose");
        post(
            &d,
            s,
            json!({"hook_event_name": "SubagentStart", "agent_id": "a471"}),
        )
        .await;
        let said = post(&d, s, theirs).await;
        assert!(context(&said).is_none(), "{said}");
        assert_eq!(row(&d, m.id).await.delivery, Delivery::Queued);
        // The main thread's next call reads it.
        let said = post(&d, s, done()).await;
        assert!(
            context(&said).is_some_and(|t| t.contains("Stop the deploy")),
            "{said}"
        );
    })
    .await;
}

#[tokio::test]
async fn the_breakers_warning_goes_with_the_call_beside_a_bosss_order_each_once() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.set_breaker_rules(Rules { loops: true }).await.unwrap();
        let s = working(&d, dir.path()).await;
        for _ in 0..6 {
            post(&d, s, tool("cargo test -p atlas")).await;
        }
        let (m, _) = urgent(&d, s, "Skip the slow tests.", Urgency::Order).await;
        let said = post(&d, s, done()).await;
        let text = context(&said).unwrap();
        assert!(
            text.contains("cargo test -p atlas"),
            "the breaker's: {text}"
        );
        assert!(text.contains("Skip the slow tests"), "the boss's: {text}");
        assert!(
            text.find("cargo test -p atlas") < text.find("Skip the slow tests"),
            "in the order they came: {text}"
        );
        assert_eq!(row(&d, m.id).await.delivery, Delivery::Delivered);
        // Each once.
        assert_eq!(post(&d, s, done()).await, json!({}));
    })
    .await;
}

#[tokio::test]
async fn a_session_that_cant_take_words_mid_turn_gets_them_at_its_end_and_the_boss_is_told() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        // No tool hook heard from it yet.
        let s = new_session(&d, dir.path()).await;
        post(&d, s.id, prompt()).await;
        let (m, rushed) = urgent(&d, s.id, "Stop.", Urgency::Order).await;
        assert_eq!(rushed, Rushed::CantSteer(Unsteerable::NoHooks));
        assert_eq!(
            rushed.said().as_deref(),
            Some("it gets it once its turn ends: it can't take words mid-turn")
        );
        assert_eq!(row(&d, m.id).await.delivery, Delivery::Queued);

        // Waiting on a dialog.
        let t = working(&d, dir.path()).await;
        post(
            &d,
            t,
            json!({"hook_event_name": "Notification", "message": "Claude needs your permission"}),
        )
        .await;
        assert_eq!(d.view(t).unwrap().state, SessionState::Blocked);
        let (_, rushed) = urgent(&d, t, "Stop.", Urgency::Order).await;
        assert_eq!(rushed, Rushed::CantSteer(Unsteerable::Dialog));

        // Not working: it goes in now, as any message.
        let idle = new_session(&d, dir.path()).await;
        d.plugin_heard_from(idle.id);
        let (m, rushed) = urgent(&d, idle.id, "Start on the docs.", Urgency::Order).await;
        assert_eq!(rushed, Rushed::NotWorking);
        assert_eq!(rushed.said(), None);
        assert_eq!(row(&d, m.id).await.urgency, None, "not kept as urgent");
        let next = d.line_next(idle.id, Duration::from_secs(5)).await.unwrap();
        assert_eq!(next["id"], json!(m.id));
    })
    .await;
}

#[test]
fn only_claude_code_devin_and_codex_take_a_bosss_words_mid_turn() {
    use crate::engine::Engine;
    use line::{mid_turn, MidTurn};
    let working = SessionState::Working;
    assert_eq!(
        mid_turn(Engine::Claude, working, true, false),
        Ok(Some(MidTurn::Hook))
    );
    assert_eq!(
        mid_turn(Engine::Devin, working, true, false),
        Ok(Some(MidTurn::Hook))
    );
    assert_eq!(
        mid_turn(Engine::Codex, working, true, false),
        Ok(Some(MidTurn::TurnSteer))
    );
    for e in [
        Engine::Gemini,
        Engine::Copilot,
        Engine::OpenCode,
        Engine::Cursor,
        Engine::Custom,
    ] {
        assert_eq!(
            mid_turn(e, working, true, false),
            Err(Unsteerable::Engine),
            "{e:?}"
        );
    }
    assert_eq!(
        mid_turn(Engine::Claude, working, false, false),
        Err(Unsteerable::NoHooks)
    );
    assert_eq!(
        mid_turn(Engine::Claude, working, true, true),
        Err(Unsteerable::Dialog)
    );
    for s in [SessionState::Blocked, SessionState::Asking] {
        assert_eq!(
            mid_turn(Engine::Claude, s, true, false),
            Err(Unsteerable::Dialog)
        );
    }
    for s in [
        SessionState::Idle,
        SessionState::Done,
        SessionState::Starting,
        SessionState::Stopped,
    ] {
        assert_eq!(mid_turn(Engine::Gemini, s, false, false), Ok(None), "{s:?}");
    }
}

#[tokio::test]
async fn past_three_urgent_orders_in_ten_minutes_they_queue_and_stand_downs_dont_count() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = working(&d, dir.path()).await;
        for i in 0..line::URGENT_PER_SESSION {
            let (_, rushed) = urgent(&d, s, &format!("Correction {i}."), Urgency::Order).await;
            assert_eq!(rushed, Rushed::Steered, "{i}");
        }
        let (m, rushed) = urgent(&d, s, "Correction 3.", Urgency::Order).await;
        assert_eq!(rushed, Rushed::TooMany);
        assert!(rushed
            .said()
            .unwrap()
            .contains("3 urgent orders in the last 10 minutes"));
        assert_eq!(row(&d, m.id).await.urgency, None);
        // A stand-down isn't held by it.
        let (_, rushed) = urgent(&d, s, "Stand down from a:1.", Urgency::StandDown).await;
        assert_eq!(rushed, Rushed::Steered);
        let (_, rushed) = urgent(&d, s, "Stand down from a:2.", Urgency::Escalate).await;
        assert_eq!(rushed, Rushed::Steered);
        // The one past the limit waits for the turn's end; the rest go now.
        let said = post(&d, s, done()).await;
        let text = context(&said).unwrap();
        assert!(
            text.contains("Correction 2.") && text.contains("a:2"),
            "{text}"
        );
        assert!(!text.contains("Correction 3."), "{text}");
        assert_eq!(row(&d, m.id).await.delivery, Delivery::Queued);
        // Another session has a limit of its own.
        let t = working(&d, dir.path()).await;
        assert_eq!(
            urgent(&d, t, "Hold.", Urgency::Order).await.1,
            Rushed::Steered
        );
        // Only a boss gives words urgently.
        assert!(d
            .line_send_with(Sender::Person, tell(s, "now"), Some(Urgency::Order))
            .await
            .is_err());
    })
    .await;
}

/// A hub whose boss interrupts as `crew.interrupt` does, when let.
struct Interrupting {
    desk: Mutex<Option<Desk>>,
    let_it: bool,
    asked: Mutex<Vec<(String, Uuid, Uuid)>>,
}

#[async_trait::async_trait]
impl Crew for Interrupting {
    async fn steer_overdue(
        &self,
        boss: &str,
        session: Uuid,
        row: Uuid,
        turn: crate::line::TurnMark,
    ) -> Result<String, String> {
        self.asked.lock().push((boss.to_string(), session, row));
        if !self.let_it {
            return Err("the person hasn't let you interrupt sessions".into());
        }
        let d = self.desk.lock().clone().unwrap();
        let v = d
            .interrupt_turn(
                session,
                turn,
                row,
                Some(By::manager(boss, "Sarah", "Its stand-down wasn't read.")),
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(format!("interrupted {}", v.label()))
    }
    fn overseer(&self) -> Option<Overseer> {
        None
    }
    fn agents(&self) -> Vec<CrewAgent> {
        Vec::new()
    }
    fn midnight(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }
    async fn accept(&self, _: &Suggestion) -> Result<(), String> {
        Ok(())
    }
}

fn interrupting(d: &Desk, let_it: bool) -> Arc<Interrupting> {
    let crew = Arc::new(Interrupting {
        desk: Mutex::new(Some(d.clone())),
        let_it,
        asked: Mutex::default(),
    });
    let dynamic: Arc<dyn Crew> = crew.clone();
    d.answer_lines_with(&dynamic);
    // Kept alive by the test: the desk holds it weakly.
    std::mem::forget(dynamic);
    crew
}

#[tokio::test]
async fn an_escalating_stand_down_not_read_in_time_interrupts_its_turn_and_goes_in_at_the_idle() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.set_steer_wait(Duration::from_millis(300));
        let crew = interrupting(&d, true);
        let s = working(&d, dir.path()).await;
        // A plain urgent order, and a stand-down that isn't of an
        // irreversible job: never escalated.
        let (plain, _) = urgent(&d, s, "Use the staging DB.", Urgency::Order).await;
        let (soft, _) = urgent(&d, s, "Stand down from a:1.", Urgency::StandDown).await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(crew.asked.lock().is_empty());
        assert_eq!(d.view(s).unwrap().state, SessionState::Working);

        // One that escalates, its tool still running past the wait.
        let (hard, _) = urgent(
            &d,
            s,
            "Stand down from a:2: don't dispatch.",
            Urgency::Escalate,
        )
        .await;
        for _ in 0..100 {
            if !crew.asked.lock().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            crew.asked.lock().clone(),
            vec![("sarah".to_string(), s, hard.id)]
        );
        assert_eq!(d.view(s).unwrap().state, SessionState::Done, "interrupted");
        // The idle after it takes what waited, in order, one at a time.
        let mut handed = Vec::new();
        for _ in 0..3 {
            let next = d
                .line_next(s, Duration::from_secs(5))
                .await
                .expect("at the idle");
            let id: Uuid = serde_json::from_value(next["id"].clone()).unwrap();
            d.line_ack(s, id, true).await.unwrap();
            handed.push(id);
        }
        assert_eq!(handed, vec![plain.id, soft.id, hard.id]);
        assert!(
            d.line_next(s, Duration::from_millis(200)).await.is_none(),
            "once each"
        );
    })
    .await;
}

#[tokio::test]
async fn an_escalating_stand_down_read_in_time_or_whose_turn_ended_interrupts_nothing() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.set_steer_wait(Duration::from_millis(300));
        let crew = interrupting(&d, true);
        let s = working(&d, dir.path()).await;
        urgent(&d, s, "Stand down from a:2.", Urgency::Escalate).await;
        assert!(context(&post(&d, s, done()).await).is_some());
        // Its turn ended before the wait was out: at the idle, no Esc.
        let t = working(&d, dir.path()).await;
        urgent(&d, t, "Stand down from a:3.", Urgency::Escalate).await;
        post(&d, t, stop()).await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(crew.asked.lock().is_empty(), "{:?}", crew.asked.lock());
    })
    .await;
}

#[tokio::test]
async fn a_boss_not_let_to_interrupt_leaves_the_stand_down_for_the_turns_end_and_says_so() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.set_steer_wait(Duration::from_millis(200));
        let crew = interrupting(&d, false);
        let s = working(&d, dir.path()).await;
        let (m, _) = urgent(&d, s, "Stand down from a:2.", Urgency::Escalate).await;
        for _ in 0..100 {
            if !crew.asked.lock().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(crew.asked.lock().len(), 1);
        assert_eq!(d.view(s).unwrap().state, SessionState::Working);
        // Still for its next call, should one come.
        let said = post(&d, s, done()).await;
        assert!(context(&said).is_some_and(|t| t.contains("a:2")), "{said}");
        assert_eq!(row(&d, m.id).await.delivery, Delivery::Delivered);
    })
    .await;
}

/// As a restart finds `s`: its turn still runs, its steer queue and what
/// was heard of it gone with the last hub; then what is queued urgently is
/// read back.
async fn restarted(d: &Desk, s: Uuid) {
    {
        let mut st = d.0.state.lock();
        let l = st.live.get_mut(&s).unwrap();
        l.breaker.steers.clear();
        l.breaker.heard = false;
    }
    d.rush_load().await;
}

#[tokio::test]
async fn an_urgent_order_a_restart_left_queued_is_held_for_the_turn_and_read_with_its_first_call() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = working(&d, dir.path()).await;
        let (m, _) = urgent(&d, s, "Don't merge PR 711.", Urgency::Order).await;
        restarted(&d, s).await;
        // Desk's plugin, waiting on the line before any hook is heard, is
        // never handed it while the turn runs.
        assert!(d.line_next(s, Duration::from_millis(300)).await.is_none());
        assert_eq!(row(&d, m.id).await.delivery, Delivery::Queued);
        // The first call heard carries it.
        let said = post(&d, s, done()).await;
        let text = context(&said).expect("with the first call's result");
        assert!(text.contains("Don't merge PR 711"), "{text}");
        assert_eq!(row(&d, m.id).await.delivery, Delivery::Delivered);
        assert_eq!(context(&post(&d, s, done()).await), None, "once");
    })
    .await;
}

#[tokio::test]
async fn an_escalating_stand_down_a_restart_left_inside_a_long_tool_is_timed_again_at_once() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let crew = interrupting(&d, true);
        let s = working(&d, dir.path()).await;
        // Sent before the restart, its wait the last hub's.
        let (hard, _) = urgent(&d, s, "Stand down from a:2.", Urgency::Escalate).await;
        d.set_steer_wait(Duration::from_millis(300));
        restarted(&d, s).await;
        // Inside `gh run watch`, no hook is heard: the plugin leaves it,
        // and the wait runs out all the same.
        assert!(d.line_next(s, Duration::from_millis(100)).await.is_none());
        for _ in 0..100 {
            if !crew.asked.lock().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            crew.asked.lock().clone(),
            vec![("sarah".to_string(), s, hard.id)]
        );
        assert_eq!(d.view(s).unwrap().state, SessionState::Done, "interrupted");
        let next = d
            .line_next(s, Duration::from_secs(5))
            .await
            .expect("at the idle");
        assert_eq!(next["id"], json!(hard.id));
    })
    .await;
}

#[tokio::test]
async fn desks_plugin_waiting_as_an_urgent_order_is_kept_is_never_handed_it_mid_turn() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = working(&d, dir.path()).await;
        let waiting = tokio::spawn({
            let d = d.clone();
            async move { d.line_next(s, Duration::from_millis(600)).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (m, rushed) = urgent(&d, s, "Use the staging DB.", Urgency::Order).await;
        assert_eq!(rushed, Rushed::Steered);
        assert!(waiting.await.unwrap().is_none(), "left for the hook");
        assert_eq!(row(&d, m.id).await.delivery, Delivery::Queued);
        let said = post(&d, s, done()).await;
        assert!(context(&said).is_some_and(|t| t.contains("staging DB")));
    })
    .await;
}

#[tokio::test]
async fn four_urgent_orders_sent_at_once_pass_the_limit_three_times() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = working(&d, dir.path()).await;
        let (a, b, c, e) = tokio::join!(
            urgent(&d, s, "A.", Urgency::Order),
            urgent(&d, s, "B.", Urgency::Order),
            urgent(&d, s, "C.", Urgency::Order),
            urgent(&d, s, "D.", Urgency::Order),
        );
        let rushed = [a.1, b.1, c.1, e.1];
        let steered = rushed.iter().filter(|r| **r == Rushed::Steered).count();
        let queued = rushed.iter().filter(|r| **r == Rushed::TooMany).count();
        assert_eq!((steered, queued), (3, 1), "{rushed:?}");
    })
    .await;
}

#[tokio::test]
async fn a_stand_down_whose_turn_ended_never_interrupts_the_turn_after() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.set_steer_wait(Duration::from_millis(400));
        let crew = interrupting(&d, true);
        let s = working(&d, dir.path()).await;
        let sent_in = d.turn_mark(s).expect("in a turn");
        let (hard, _) = urgent(&d, s, "Stand down from a:2.", Urgency::Escalate).await;
        // Its turn ends, and a new prompt starts a long tool, all within the
        // wait.
        post(&d, s, stop()).await;
        post(&d, s, prompt()).await;
        post(&d, s, tool("cargo build --release")).await;
        assert_eq!(d.view(s).unwrap().state, SessionState::Working);
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(crew.asked.lock().is_empty(), "{:?}", crew.asked.lock());
        assert_eq!(d.view(s).unwrap().state, SessionState::Working);
        // An interrupt for it, late, is refused: that turn is over.
        assert!(d.interrupt_turn(s, sent_in, hard.id, None).await.is_err());
        assert_eq!(d.view(s).unwrap().state, SessionState::Working);
    })
    .await;
}

#[tokio::test]
async fn esc_in_a_dialog_leaves_the_bosss_order_for_the_idle_not_the_stopped_call() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = working(&d, dir.path()).await;
        let (m, _) = urgent(&d, s, "Use the staging DB.", Urgency::Order).await;
        post(
            &d,
            s,
            json!({"hook_event_name": "Notification", "message": "Claude needs your permission"}),
        )
        .await;
        assert_eq!(d.view(s).unwrap().state, SessionState::Blocked);
        let stopped = post(
            &d,
            s,
            json!({"hook_event_name": "PostToolUseFailure", "tool_name": "Bash",
                "tool_input": {"command": "gh run watch"}, "error": "Interrupted",
                "is_interrupt": true}),
        )
        .await;
        assert!(
            !context(&stopped).is_some_and(|t| t.contains("staging DB")),
            "{stopped}"
        );
        let next = d
            .line_next(s, Duration::from_secs(5))
            .await
            .expect("at the idle");
        assert_eq!(next["id"], json!(m.id));
    })
    .await;
}

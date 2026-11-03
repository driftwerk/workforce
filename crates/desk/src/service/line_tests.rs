//! Desk's lines end to end in the desk: sends, waits, hand-offs to Desk's
//! plugin, typing in at the prompt, the loop guard, and the person's own
//! messages.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::super::tests::{desk, new_session, options};
use super::*;
use crate::crew::By;
use crate::line::{Delivery, LineKind};
use crate::model::{Engine, NewSession, SessionState};
use crate::reads::Urgency;
use crate::service::Options;
use crate::store::{build_pool, DeskStore};

async fn hook(d: &Desk, id: Uuid, body: Value) -> Value {
    let token = d.0.state.lock().live[&id].token.clone();
    d.hook(id, &token, body.to_string().as_bytes())
        .await
        .unwrap()
}

/// It starts, and Desk's plugin in it asks for the line's tools, as it
/// does at its start.
async fn idle(d: &Desk, id: Uuid) {
    hook(
        d,
        id,
        json!({"hook_event_name": "SessionStart", "session_id": id.to_string()}),
    )
    .await;
    d.plugin_heard_from(id);
}

async fn until(mut ok: impl FnMut() -> bool) {
    for _ in 0..500 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("not in time");
}

async fn until_async<F: std::future::Future<Output = bool>>(mut ok: impl FnMut() -> F) {
    for _ in 0..500 {
        if ok().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("not in time");
}

async fn row(d: &Desk, id: Uuid) -> LineMessage {
    d.0.store.line_message(id).await.unwrap().unwrap()
}

fn tell(to: Uuid, text: &str) -> Send {
    Send::Tell {
        to: Target::Session(to),
        text: text.into(),
        when: When::Queue,
    }
}

#[tokio::test]
async fn a_question_for_desk_with_no_overseer_reaches_the_person_and_their_answer_is_the_tools_result(
) {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        let asking = {
            let d = d.clone();
            tokio::spawn(async move {
                d.line_call(
                    s.id,
                    "ask",
                    &json!({"to": "desk", "text": "Which port does the test database use?"}),
                    None,
                )
                .await
            })
        };
        let mut ask = None;
        until_async(|| {
            let d = d.clone();
            async move {
                !d.0.store
                    .line_open_asks(Some("you"))
                    .await
                    .unwrap()
                    .is_empty()
            }
        })
        .await;
        for q in d.0.store.line_open_asks(Some("you")).await.unwrap() {
            ask = Some(q);
        }
        let ask = ask.unwrap();
        assert_eq!(ask.to.kind, PartyKind::You, "no overseer: the person");
        assert_eq!(ask.from.id, s.id.to_string());
        let answer = d
            .line_answer(ask.id, Sender::Person, "5433".into())
            .await
            .unwrap();
        let result = asking.await.unwrap().unwrap();
        assert_eq!(result["answer"], "5433");
        assert_eq!(
            row(&d, answer.id).await.delivery,
            Delivery::Delivered,
            "it went back as the result"
        );
        assert_eq!(row(&d, ask.id).await.asked, Some(Asked::Answered));
        assert!(matches!(
            d.line_answer(ask.id, Sender::Person, "5432".into()).await,
            Err(LineError::Refused(Refusal::Answered))
        ));
        // The person answering resets its loop guard.
        assert_eq!(d.line_run(s.id).sent, 0);
    })
    .await;
}

#[tokio::test]
async fn an_unanswered_question_says_pending_and_its_answer_comes_later_as_a_message() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        let result = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "you", "text": "Ship it?", "wait_seconds": 0}),
                None,
            )
            .await
            .unwrap();
        assert!(result["pending"].as_str().unwrap().contains("No answer"));
        assert_eq!(result["more"], false);
        let ask: Uuid = serde_json::from_value(result["id"].clone()).unwrap();
        // The plugin asks again while a longer wait has time left.
        let result = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "you", "text": "Tests too?", "wait_seconds": 60}),
                Some(Duration::from_millis(50)),
            )
            .await
            .unwrap();
        assert_eq!(result["more"], true);
        let answer = d
            .line_answer(ask, Sender::Person, "Yes".into())
            .await
            .unwrap();
        let next = d.line_next(s.id, Duration::from_secs(2)).await.unwrap();
        assert_eq!(next["id"], json!(answer.id));
        assert!(next["text"]
            .as_str()
            .unwrap()
            .starts_with("You asked me “Ship it?”. Yes"));
    })
    .await;
}

#[tokio::test]
async fn desks_plugin_is_handed_one_message_at_a_time_and_a_dropped_one_goes_again() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        let first = d
            .line_post(s.id, "run the e2e tests".into(), When::Queue)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            row(&d, first.id).await.delivery,
            Delivery::Queued,
            "left for the plugin"
        );
        let second = d
            .line_post(s.id, "then push".into(), When::Queue)
            .await
            .unwrap();
        let next = d.line_next(s.id, Duration::from_secs(1)).await.unwrap();
        assert_eq!(next["id"], json!(first.id));
        assert_eq!(
            next["text"], "run the e2e tests",
            "the person's words as written"
        );
        assert!(
            d.line_next(s.id, Duration::from_millis(50)).await.is_none(),
            "one on its way at a time"
        );
        // Edited only until it is handed.
        assert!(matches!(
            d.line_edit(first.id, Some("x".into())).await,
            Err(DeskError::Conflict(_))
        ));
        d.line_edit(second.id, Some("then open the PR".into()))
            .await
            .unwrap();
        d.line_ack(s.id, first.id, false).await.unwrap();
        assert_eq!(
            row(&d, first.id).await.delivery,
            Delivery::Queued,
            "dropped: again"
        );
        let again = d.line_next(s.id, Duration::from_secs(1)).await.unwrap();
        assert_eq!(again["id"], json!(first.id));
        d.line_ack(s.id, first.id, true).await.unwrap();
        assert_eq!(row(&d, first.id).await.delivery, Delivery::Delivered);
        let next = d.line_next(s.id, Duration::from_secs(1)).await.unwrap();
        assert_eq!(next["text"], "then open the PR");
        // Another session's ack is no one's.
        let other = new_session(&d, dir.path()).await;
        assert!(d.line_ack(other.id, second.id, true).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn a_message_to_a_stopped_session_is_held_until_it_runs_again() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let a = new_session(&d, dir.path()).await;
        let b = new_session(&d, dir.path()).await;
        idle(&d, a.id).await;
        d.stop(b.id, None).unwrap();
        until(|| !d.view(b.id).unwrap().live).await;
        let r = d
            .line_call(
                a.id,
                "tell",
                &json!({"to": b.id, "text": "the API moved to /v2"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(r["delivery"], "held");
        assert!(r["note"]
            .as_str()
            .unwrap()
            .contains("it gets this when it runs again"));
        let id: Uuid = serde_json::from_value(r["id"].clone()).unwrap();
        let thread = d.line_thread(b.id, None, None).await.unwrap();
        assert_eq!(thread.last().unwrap().waiting, Some(Wait::Stopped));
        d.resume(b.id, None, None).await.unwrap();
        until_async(|| {
            let d = d.clone();
            async move { row(&d, id).await.delivery == Delivery::Queued }
        })
        .await;
        let next = d.line_next(b.id, Duration::from_secs(1)).await.unwrap();
        assert!(next["text"]
            .as_str()
            .unwrap()
            .contains("(a Claude Code session, hop 1) tells you: the API moved to /v2"));
    })
    .await;
}

#[tokio::test]
async fn the_loop_guard_holds_a_session_to_its_budget_until_the_person_speaks_and_a_delivery_isnt_them(
) {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let a = new_session(&d, dir.path()).await;
        idle(&d, a.id).await;
        for i in 0..crate::rooms::TURN_BUDGET {
            d.line_call(
                a.id,
                "tell",
                &json!({"to": "you", "text": format!("step {i}")}),
                None,
            )
            .await
            .unwrap();
        }
        let refused = d
            .line_call(
                a.id,
                "tell",
                &json!({"to": "you", "text": "one more"}),
                None,
            )
            .await;
        assert_eq!(refused, Err(Refusal::Budget.to_string()));
        // A message Desk's plugin submitted is not the person speaking.
        let m = d
            .line_post(a.id, "carry on".into(), When::Queue)
            .await
            .unwrap();
        assert_eq!(d.line_run(a.id).sent, 0, "the person's message resets it");
        d.0.state.lock().lines.runs.entry(a.id).or_default().sent = 5;
        hook(
            &d,
            a.id,
            json!({"hook_event_name": "UserPromptSubmit",
            "prompt": format!("The desk plugin sent a message: {} hi", line::marker(m.id))}),
        )
        .await;
        assert_eq!(d.line_run(a.id).sent, 5);
        assert_eq!(d.view(a.id).unwrap().state, SessionState::Working);
        hook(
            &d,
            a.id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "go on"}),
        )
        .await;
        assert_eq!(
            d.line_run(a.id).sent,
            0,
            "a prompt of the person's resets it"
        );
    })
    .await;
}

#[tokio::test]
async fn a_hand_off_chain_is_refused_past_its_hops() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let a = new_session(&d, dir.path()).await;
        let b = new_session(&d, dir.path()).await;
        idle(&d, a.id).await;
        idle(&d, b.id).await;
        let ab = d
            .line_send(Sender::Session(a.id), tell(b.id, "one"))
            .await
            .unwrap();
        assert_eq!(ab.hop, 1);
        d.line_next(b.id, Duration::from_secs(1)).await.unwrap();
        d.line_ack(b.id, ab.id, true).await.unwrap();
        let ba = d
            .line_send(Sender::Session(b.id), tell(a.id, "two"))
            .await
            .unwrap();
        assert_eq!(ba.hop, 2);
        d.line_next(a.id, Duration::from_secs(1)).await.unwrap();
        d.line_ack(a.id, ba.id, true).await.unwrap();
        let ab = d
            .line_send(Sender::Session(a.id), tell(b.id, "three"))
            .await
            .unwrap();
        assert_eq!(ab.hop, 3);
        d.line_next(b.id, Duration::from_secs(1)).await.unwrap();
        d.line_ack(b.id, ab.id, true).await.unwrap();
        assert!(matches!(
            d.line_send(Sender::Session(b.id), tell(a.id, "four")).await,
            Err(LineError::Refused(Refusal::Hops))
        ));
        // As the refusal says, the person can still be asked.
        let asked = d
            .line_send(
                Sender::Session(b.id),
                Send::Ask {
                    to: Target::Person,
                    text: "stuck".into(),
                    wait: Duration::ZERO,
                },
            )
            .await
            .unwrap();
        assert_eq!(asked.to.kind, PartyKind::You);
    })
    .await;
}

#[tokio::test]
async fn forgetting_a_session_withdraws_what_was_asked_of_it_and_tells_the_asker() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let a = new_session(&d, dir.path()).await;
        let b = new_session(&d, dir.path()).await;
        idle(&d, a.id).await;
        let r = d
            .line_call(
                a.id,
                "ask",
                &json!({"to": b.id, "text": "which branch?", "wait_seconds": 0}),
                None,
            )
            .await
            .unwrap();
        let ask: Uuid = serde_json::from_value(r["id"].clone()).unwrap();
        let mine = d
            .line_post(b.id, "hello".into(), When::Queue)
            .await
            .unwrap();
        d.forget(b.id).await.unwrap();
        assert_eq!(
            row(&d, ask).await.asked,
            Some(Asked::Withdrawn),
            "kept: it is in a's thread"
        );
        until_async(|| {
            let d = d.clone();
            async move { d.0.store.line_message(mine.id).await.unwrap().is_none() }
        })
        .await;
        let next = d.line_next(a.id, Duration::from_secs(1)).await.unwrap();
        assert!(next["text"]
            .as_str()
            .unwrap()
            .contains("has ended, so your question “which branch?” to it is withdrawn"));
    })
    .await;
}

#[tokio::test]
async fn sessions_reach_the_ones_the_reach_setting_lets_them() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let d = Desk::start(
            DeskStore::from_pool(pool),
            Options {
                line_reach: Reach::Overseer,
                ..options(dir.path())
            },
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let a = new_session(&d, dir.path()).await;
        let b = new_session(&d, dir.path()).await;
        let refused = d
            .line_call(a.id, "tell", &json!({"to": b.id, "text": "hi"}), None)
            .await;
        assert!(refused
            .unwrap_err()
            .contains("sessions message only Desk and the person here"));
        let peers = d.line_call(a.id, "peers", &json!({}), None).await.unwrap();
        assert_eq!(peers["sessions"], json!([]));
        assert!(peers["desk"]
            .as_str()
            .unwrap()
            .contains("reaches the person"));
        let told = d
            .line_call(a.id, "tell", &json!({"to": "you", "text": "done"}), None)
            .await
            .unwrap();
        assert_eq!(told["delivery"], "delivered", "shown to the person at once");
    })
    .await;
}

#[tokio::test]
async fn a_session_that_wrote_to_the_person_is_unread_for_them() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let a = new_session(&d, dir.path()).await;
        d.line_call(
            a.id,
            "tell",
            &json!({"to": "you", "text": "PR is up"}),
            None,
        )
        .await
        .unwrap();
        let points = HashMap::new();
        let unread = d.sessions_unread(&points, chrono::DateTime::<Utc>::MIN_UTC);
        let mine = unread.iter().find(|u| u.id == a.id).unwrap();
        assert_eq!(mine.urgency, Urgency::ForYou);
    })
    .await;
}

#[tokio::test]
async fn send_now_types_into_a_working_claude_session_and_is_refused_from_a_session() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let a = new_session(&d, dir.path()).await;
        hook(
            &d,
            a.id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "build it"}),
        )
        .await;
        let m = d
            .line_post(a.id, "use the staging DB".into(), When::Now)
            .await
            .unwrap();
        assert_eq!(m.delivery, Delivery::Delivered);
        until(|| {
            d.terminal_text(a.id, 2000)
                .unwrap()
                .contains("use the staging DB")
        })
        .await;
        let b = new_session(&d, dir.path()).await;
        assert!(matches!(
            d.line_send(
                Sender::Session(b.id),
                Send::Tell {
                    to: Target::Session(a.id),
                    text: "x".into(),
                    when: When::Now
                }
            )
            .await,
            Err(LineError::Refused(Refusal::NotYours))
        ));
    })
    .await;
}

/// A stand-in `codex`, as `codex_asks_tests` has it: it says where its
/// app-server was told to listen, and what else it was told.
fn fake_codex(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-codex");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n[ \"$1\" = app-server ] && echo \"${{3#unix://}}\" > '{d}/listen' && echo \"$@\" > '{d}/server-args'\nexec sleep 60\n",
            d = dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn serve(
    dir: &Path,
) -> tokio::task::JoinHandle<tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>> {
    let told = dir.join("listen");
    tokio::spawn(async move {
        let socket = loop {
            if let Ok(s) = std::fs::read_to_string(&told) {
                if !s.trim().is_empty() {
                    break PathBuf::from(s.trim());
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        tokio_tungstenite::accept_async(stream).await.unwrap()
    })
}

#[tokio::test]
async fn a_codex_session_gets_messages_typed_in_at_idle_while_the_person_isnt_typing_and_steers_mid_turn(
) {
    use crate::codex::tests::{next_sent, say};
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let d = Desk::start(
            DeskStore::from_pool(pool),
            options(dir.path()).with_program(Engine::Codex, fake_codex(dir.path())),
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let server = serve(dir.path());
        let s = d
            .create(
                NewSession {
                    engine: Engine::Codex,
                    view: None,
                    name: "acme-api".into(),
                    cwd: dir.path().to_string_lossy().into_owned(),
                    goal: String::new(),
                    worktree: None,
                    prompt: None,
                    approvals: None,
                    approve_for_me: false,
                    account: None,
                    env: Default::default(),
                },
                None,
            )
            .await
            .unwrap();
        // Its app-server serves Desk's line to it, as MCP.
        let args = std::fs::read_to_string(dir.path().join("server-args")).unwrap();
        let token = d.0.state.lock().live[&s.id].token.clone();
        assert!(args.contains(&format!("mcp_servers.desk.url=\"{}/mcp/{}/{token}\"", d.0.hook_base, s.id)), "{args}");
        assert!(args.contains("mcp_servers.desk.tool_timeout_sec=330"));
        let mut ws = server.await.unwrap();
        next_sent(&mut ws, |v| v["method"] == "initialize").await;
        let status = |kind: &str| json!({"method": "thread/status/changed", "params": {"threadId": "t1", "status": {"type": kind, "activeFlags": []}}});
        say(&mut ws, json!({"method": "thread/started", "params": {"thread": {"id": "t1", "ephemeral": false}}})).await;
        say(&mut ws, status("active")).await;
        say(&mut ws, json!({"method": "turn/started", "params": {"threadId": "t1", "turn": {"id": "u1"}}})).await;
        until(|| d.view(s.id).unwrap().state == SessionState::Working).await;

        let waits = d.line_post(s.id, "then run the linter".into(), When::Queue).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(row(&d, waits.id).await.delivery, Delivery::Queued, "it works: it waits");
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        assert_eq!(thread.last().unwrap().waiting, Some(Wait::Working));

        // Send now goes into the running turn.
        let now = d.line_post(s.id, "skip the slow tests".into(), When::Now).await.unwrap();
        let steer = next_sent(&mut ws, |v| v["method"] == "turn/steer").await;
        assert_eq!(steer["params"]["expectedTurnId"], "u1");
        assert_eq!(steer["params"]["input"][0]["text"], "skip the slow tests");
        assert_eq!(row(&d, now.id).await.delivery, Delivery::Delivered);
        // Codex refused it (the turn had ended): it waits for the idle.
        say(&mut ws, json!({"id": 4, "error": {"code": -32600, "message": "no active turn"}})).await;
        until_async(|| {
            let d = d.clone();
            async move { row(&d, now.id).await.delivery == Delivery::Queued }
        })
        .await;

        // The person typing when it goes idle holds what waits.
        let pty = d.attach(s.id).unwrap();
        let (viewer, _, _) = pty.attach();
        viewer.claim();
        viewer.input(b"hm").unwrap();
        say(&mut ws, json!({"method": "turn/completed", "params": {"threadId": "t1", "turn": {"id": "u1"}}})).await;
        say(&mut ws, status("idle")).await;
        until(|| d.view(s.id).unwrap().state != SessionState::Working).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(row(&d, waits.id).await.delivery, Delivery::Queued, "you're typing");
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        assert!(thread.iter().any(|m| m.waiting == Some(Wait::Typing)));

        // They send what they typed: its turn's end takes the first that
        // waits.
        say(&mut ws, json!({"method": "item/started", "params": {"threadId": "t1",
            "item": {"type": "userMessage", "content": [{"type": "text", "text": "hm, also the docs"}]}}})).await;
        until(|| d.view(s.id).unwrap().last_prompt.as_deref() == Some("hm, also the docs")).await;
        say(&mut ws, status("active")).await;
        until(|| d.view(s.id).unwrap().state == SessionState::Working).await;
        say(&mut ws, status("idle")).await;
        until_async(|| {
            let d = d.clone();
            async move { row(&d, waits.id).await.delivery == Delivery::Delivered }
        })
        .await;
        assert_eq!(row(&d, now.id).await.delivery, Delivery::Queued, "one at a time");
        until(|| d.terminal_text(s.id, 4000).unwrap().contains("then run the linter")).await;
        // Typed in, it comes back as its prompt, which isn't the person's.
        let prompts = d.view(s.id).unwrap().prompts;
        say(&mut ws, json!({"method": "item/started", "params": {"threadId": "t1",
            "item": {"type": "userMessage", "content": [{"type": "text", "text": "then run the linter"}]}}})).await;
        until(|| d.view(s.id).unwrap().last_prompt.as_deref() == Some("then run the linter")).await;
        assert_eq!(d.view(s.id).unwrap().prompts, prompts + 1, "the person's own words count");
    })
    .await;
}

#[tokio::test]
async fn a_claude_session_without_desks_plugin_is_typed_into_at_its_prompt() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        hook(
            &d,
            s.id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "build it"}),
        )
        .await;
        let m = d
            .line_post(s.id, "then the docs".into(), When::Queue)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            row(&d, m.id).await.delivery,
            Delivery::Queued,
            "it works: it waits"
        );
        hook(
            &d,
            s.id,
            json!({"hook_event_name": "Stop", "last_assistant_message": "Built."}),
        )
        .await;
        until_async(|| {
            let d = d.clone();
            async move { row(&d, m.id).await.delivery == Delivery::Delivered }
        })
        .await;
        until(|| {
            d.terminal_text(s.id, 2000)
                .unwrap()
                .contains("then the docs")
        })
        .await;
    })
    .await;
}

#[tokio::test]
async fn a_question_given_to_the_person_is_a_floor_need_until_it_is_answered() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        let r = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "you", "text": "Which branch?", "wait_seconds": 0}),
                None,
            )
            .await
            .unwrap();
        let ask: Uuid = serde_json::from_value(r["id"].clone()).unwrap();
        let needs = d.line_needs();
        assert_eq!(needs.len(), 1, "asked of the person");
        assert_eq!(needs[0].id, format!("line:{ask}"));
        assert_eq!(needs[0].who.id, s.id.to_string());
        let by = By::manager("sarah", "Sarah", "Pushing is the person's.");
        let given = d
            .line_escalate(
                ask,
                &by,
                line::Escalation {
                    why: "Pushing is the person's.".into(),
                    options: vec!["main".into(), "release".into()],
                    draft: Some("main".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(given.asked, Some(Asked::Escalated));
        assert_eq!(
            d.line_needs()[0].options,
            Some(vec!["main".into(), "release".into()])
        );
        d.line_answer(ask, Sender::Person, "main".into())
            .await
            .unwrap();
        assert!(d.line_needs().is_empty(), "answered: no longer a need");
        assert!(matches!(
            d.line_escalate(
                ask,
                &by,
                line::Escalation {
                    why: "x".into(),
                    options: vec![],
                    draft: None
                }
            )
            .await,
            Err(LineError::Refused(Refusal::Answered))
        ));
    })
    .await;
}

#[tokio::test]
async fn the_overseers_answer_counts_against_the_session_that_asked() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        let r = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "you", "text": "Port?", "wait_seconds": 0}),
                None,
            )
            .await
            .unwrap();
        let ask: Uuid = serde_json::from_value(r["id"].clone()).unwrap();
        // A question to the person is answerable by the overseer when it was
        // asked of it: here, asked of the person, the overseer can't.
        let by = By::manager("sarah", "Sarah", "Routine.");
        assert!(matches!(
            d.line_answer(ask, Sender::Overseer(by.clone()), "5433".into())
                .await,
            Err(LineError::Refused(Refusal::NoSuchAsk(_)))
        ));
        let before = d.line_run(s.id).sent;
        let mut q = row(&d, ask).await;
        q.to = Party::manager("sarah", "Sarah");
        d.0.store.put_line(&q).await.unwrap();
        d.line_answer(ask, Sender::Overseer(by), "5433".into())
            .await
            .unwrap();
        assert_eq!(d.line_run(s.id).sent, before + 1);
    })
    .await;
}

#[test]
fn a_line_kind_reads_back() {
    for k in [LineKind::Ask, LineKind::Tell, LineKind::Answer] {
        assert_eq!(LineKind::parse(k.as_str()).unwrap(), k);
    }
}

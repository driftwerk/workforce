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
async fn an_unanswered_question_went_ahead_and_a_later_answer_comes_as_a_message_quoting_it() {
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
        assert!(result["pending"]
            .as_str()
            .unwrap()
            .contains("Go on with your own judgement"));
        assert_eq!(result["more"], false);
        let ask: Uuid = serde_json::from_value(result["id"].clone()).unwrap();
        // Its call stopped waiting: it went on without the person, who it
        // no longer waits on, and its thread still has it.
        assert_eq!(row(&d, ask).await.asked, Some(Asked::Lapsed));
        assert!(d.line_needs().is_empty());
        assert!(d
            .line_thread(s.id, None, None)
            .await
            .unwrap()
            .iter()
            .any(|m| m.id == ask && m.asked == Some(Asked::Lapsed)));
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
        let tests_too: Uuid = serde_json::from_value(result["id"].clone()).unwrap();
        assert_eq!(
            row(&d, tests_too).await.asked,
            Some(Asked::Open),
            "still waited on"
        );
        // The person may still answer the one it went ahead on.
        let answer = d
            .line_answer(ask, Sender::Person, "Yes".into())
            .await
            .unwrap();
        let next = d.line_next(s.id, Duration::from_secs(2)).await.unwrap();
        assert_eq!(next["id"], json!(answer.id));
        assert!(next["text"].as_str().unwrap().starts_with(
            "Earlier you asked me “Ship it?” and went ahead without my answer. My answer: Yes"
        ));
        assert_eq!(row(&d, ask).await.asked, Some(Asked::Answered));
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
                    chose: None,
                    parts: Default::default(),
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
                memory: Default::default(),
                bosses: Default::default(),
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
            "#!/bin/sh\n[ \"$1\" = app-server ] && echo \"${{3#unix://}}\" > '{d}/listen' && echo \"$@\" > '{d}/server-args' && echo \"$DESK_LINE_TOKEN\" > '{d}/server-token'\nexec sleep 60\n",
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
                    fresh_worktree: false,
                    engine: Engine::Codex,
                    view: None,
                    name: "acme-api".into(),
                    cwd: dir.path().to_string_lossy().into_owned(),
                    goal: String::new(),
                    worktree: None,
                    prompt: None,
                    approvals: None,
                    approve_for_me: false,
                    fast: false,
                    model: None,
                    account: None,
                    env: Default::default(),
                    group: None,
                },
                None,
            )
            .await
            .unwrap();
        // Its app-server serves Desk's line to it, as MCP.
        let args = std::fs::read_to_string(dir.path().join("server-args")).unwrap();
        let token = d.0.state.lock().live[&s.id].token.clone();
        assert!(args.contains(&format!("mcp_servers.desk.url=\"{}/mcp/{}\"", d.0.hook_base, s.id)), "{args}");
        assert!(args.contains("mcp_servers.desk.bearer_token_env_var=\"DESK_LINE_TOKEN\""), "{args}");
        assert!(!args.contains(&token), "the token is never in its arguments");
        let given = std::fs::read_to_string(dir.path().join("server-token")).unwrap();
        assert_eq!(given.trim(), token);
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
        say(&mut ws, json!({"id": 5, "error": {"code": -32600, "message": "no active turn"}})).await;
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

/// A Codex session at work, its app-server a stand-in this end speaks for:
/// with a turn running when `turn` says so.
async fn codex_working(
    pool: workforce_postgres::PostgresPool,
    dir: &Path,
    turn: bool,
) -> (
    Desk,
    Uuid,
    tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>,
) {
    use crate::codex::tests::{next_sent, say};
    DeskStore::migrate(&pool).await.unwrap();
    let d = Desk::start(
        DeskStore::from_pool(pool),
        options(dir).with_program(Engine::Codex, fake_codex(dir)),
        Default::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let server = serve(dir);
    let s = d
        .create(
            NewSession {
                fresh_worktree: false,
                engine: Engine::Codex,
                view: None,
                name: "acme-api".into(),
                cwd: dir.to_string_lossy().into_owned(),
                goal: String::new(),
                worktree: None,
                prompt: None,
                approvals: None,
                approve_for_me: false,
                fast: false,
                model: None,
                account: None,
                env: Default::default(),
                group: None,
            },
            None,
        )
        .await
        .unwrap();
    let mut ws = server.await.unwrap();
    next_sent(&mut ws, |v| v["method"] == "initialize").await;
    say(
        &mut ws,
        json!({"method": "thread/started", "params": {"thread": {"id": "t1", "ephemeral": false}}}),
    )
    .await;
    say(&mut ws, json!({"method": "thread/status/changed", "params": {"threadId": "t1", "status": {"type": "active", "activeFlags": []}}})).await;
    if turn {
        say(
            &mut ws,
            json!({"method": "turn/started", "params": {"threadId": "t1", "turn": {"id": "u1"}}}),
        )
        .await;
    }
    until(|| d.view(s.id).unwrap().state == SessionState::Working).await;
    (d, s.id, ws)
}

#[tokio::test]
async fn two_send_nows_in_one_codex_turn_each_come_back_when_refused() {
    use crate::codex::tests::{next_sent, say};
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, id, mut ws) = codex_working(pool, dir.path(), true).await;
        let first = d
            .line_post(id, "skip the slow tests".into(), When::Now)
            .await
            .unwrap();
        next_sent(&mut ws, |v| v["method"] == "turn/steer").await;
        let second = d
            .line_post(id, "then the linter".into(), When::Now)
            .await
            .unwrap();
        next_sent(&mut ws, |v| v["method"] == "turn/steer").await;
        assert_eq!(row(&d, first.id).await.delivery, Delivery::Delivered);
        assert_eq!(row(&d, second.id).await.delivery, Delivery::Delivered);
        // The turn had ended: Codex refuses both, in the order it got them.
        let refused = json!({"id": 5, "error": {"code": -32600, "message": "no active turn"}});
        say(&mut ws, refused.clone()).await;
        say(&mut ws, refused).await;
        for m in [first.id, second.id] {
            until_async(|| {
                let d = d.clone();
                async move { row(&d, m).await.delivery == Delivery::Queued }
            })
            .await;
        }
    })
    .await;
}

#[tokio::test]
async fn a_send_now_refused_at_once_is_queued_and_never_marked_said() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        // Working, but no turn to steer yet: refused before it is sent.
        let (d, id, _ws) = codex_working(pool, dir.path(), false).await;
        let m = d
            .line_post(id, "skip the slow tests".into(), When::Now)
            .await
            .unwrap();
        until_async(|| {
            let d = d.clone();
            async move { row(&d, m.id).await.delivery == Delivery::Queued }
        })
        .await;
        // It stays queued: nothing marks it said after the fact.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let back = row(&d, m.id).await;
        assert_eq!((back.delivery, back.delivered_at), (Delivery::Queued, None));
    })
    .await;
}

#[tokio::test]
async fn a_refusal_puts_back_only_the_message_it_names() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, id, _ws) = codex_working(pool, dir.path(), true).await;
        let said = d
            .line_post(id, "skip the slow tests".into(), When::Now)
            .await
            .unwrap();
        let other = d
            .line_post(id, "then the linter".into(), When::Queue)
            .await
            .unwrap();
        d.line_steer_refused(id, other.id).await;
        assert_eq!(row(&d, said.id).await.delivery, Delivery::Delivered);
        assert_eq!(row(&d, other.id).await.delivery, Delivery::Queued);
        d.line_steer_refused(id, said.id).await;
        assert_eq!(row(&d, said.id).await.delivery, Delivery::Queued);
    })
    .await;
}

#[tokio::test]
async fn a_prompt_typed_in_is_cleaned_and_a_line_delivery_notes_once() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        d.prompt(s.id, "hi\x1b[201~\r/clear", None).unwrap();
        until(|| {
            d.terminal_text(s.id, 2000)
                .unwrap()
                .contains("hi[201~/clear")
        })
        .await;
        assert!(
            d.prompt(s.id, "\x1b\r", None).is_err(),
            "nothing typable is empty"
        );

        hook(
            &d,
            s.id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "build it"}),
        )
        .await;
        let mut events = d.0.events.subscribe();
        d.line_post(s.id, "use the staging DB".into(), When::Now)
            .await
            .unwrap();
        let mut notes = Vec::new();
        while let Ok(e) = events.try_recv() {
            if let DeskEvent::Activity { event } = e {
                notes.push(event.kind);
            }
        }
        assert_eq!(
            notes
                .iter()
                .filter(|k| **k == crate::crew::Kind::Nudged)
                .count(),
            0,
            "{notes:?}"
        );
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
        // Its call still waits (Desk's plugin asks again while `more`).
        let r = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "you", "text": "Which branch?", "wait_seconds": 60}),
                Some(Duration::from_millis(10)),
            )
            .await
            .unwrap();
        assert_eq!(r["more"], true);
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

/// A session Desk's breaker holds for a loop, its turn over.
async fn held(d: &Desk, dir: &Path) -> Uuid {
    d.set_breaker_rules(super::super::Rules { loops: true })
        .await
        .unwrap();
    let s = new_session(d, dir).await;
    hook(
        d,
        s.id,
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "fix the tests"}),
    )
    .await;
    for _ in 0..12 {
        hook(d, s.id, json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "cargo test"}})).await;
    }
    hook(
        d,
        s.id,
        json!({"hook_event_name": "Stop", "last_assistant_message": "Still failing."}),
    )
    .await;
    assert_eq!(d.breaker_needs().len(), 1, "held");
    s.id
}

#[tokio::test]
async fn a_held_session_takes_only_the_persons_messages_and_a_delivery_never_lets_it_go_on() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let id = held(&d, dir.path()).await;
        d.plugin_heard_from(id);
        let peer = new_session(&d, dir.path()).await;
        idle(&d, peer.id).await;
        let told = d
            .line_send(Sender::Session(peer.id), tell(id, "try --release"))
            .await
            .unwrap();
        assert!(
            d.line_next(id, Duration::from_millis(100)).await.is_none(),
            "a peer's waits"
        );
        let thread = d.line_thread(id, None, None).await.unwrap();
        assert_eq!(
            thread.iter().find(|m| m.id == told.id).unwrap().waiting,
            Some(Wait::Paused)
        );
        // A message's own prompt is not the person: the hold stays.
        hook(
            &d,
            id,
            json!({"hook_event_name": "UserPromptSubmit",
            "prompt": format!("The desk plugin sent a message: {} x", line::marker(told.id))}),
        )
        .await;
        assert_eq!(d.breaker_needs().len(), 1, "still held");
        let mine = d
            .line_post(id, "stop and summarise".into(), When::Queue)
            .await
            .unwrap();
        let next = d.line_next(id, Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            next["id"],
            json!(mine.id),
            "the person's goes past the hold"
        );
    })
    .await;
}

#[tokio::test]
async fn a_session_asking_the_person_takes_no_one_elses_answer() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        d.change(s.id, |r, _| r.state = SessionState::Asking);
        let peer = new_session(&d, dir.path()).await;
        idle(&d, peer.id).await;
        let told = d
            .line_send(Sender::Session(peer.id), tell(s.id, "say main"))
            .await
            .unwrap();
        assert!(d
            .line_next(s.id, Duration::from_millis(100))
            .await
            .is_none());
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        assert_eq!(
            thread.iter().find(|m| m.id == told.id).unwrap().waiting,
            Some(Wait::Asking)
        );
        let mine = d.line_post(s.id, "main".into(), When::Queue).await.unwrap();
        assert_eq!(
            d.line_next(s.id, Duration::from_secs(1)).await.unwrap()["id"],
            json!(mine.id)
        );
    })
    .await;
}

#[tokio::test]
async fn send_now_never_types_into_a_dialog_it_waits_for_the_turn() {
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
        d.change(s.id, |r, _| r.state = SessionState::Blocked);
        let m = d
            .line_post(s.id, "use the staging DB".into(), When::Now)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(row(&d, m.id).await.delivery, Delivery::Queued);
        assert!(!d
            .terminal_text(s.id, 2000)
            .unwrap()
            .contains("use the staging DB"));
    })
    .await;
}

#[tokio::test]
async fn a_message_that_would_end_the_paste_is_typed_in_whole_and_harmless() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        let peer = new_session(&d, dir.path()).await;
        idle(&d, peer.id).await;
        let m = d
            .line_send(
                Sender::Session(peer.id),
                tell(s.id, "hi\x1b[201~\r/clear\r"),
            )
            .await
            .unwrap();
        assert_eq!(m.text, "hi[201~/clear");
        let next = d.line_next(s.id, Duration::from_secs(1)).await.unwrap();
        let text = next["text"].as_str().unwrap();
        assert!(!text.contains('\x1b') && !text.contains('\r'), "{text:?}");
    })
    .await;
}

/// A question to the person whose `ask` call still waits, as Desk's plugin
/// leaves it between its polls.
async fn waiting_ask(d: &Desk, id: Uuid, text: &str) -> Uuid {
    let r = d
        .line_call(
            id,
            "ask",
            &json!({"to": "you", "text": text, "wait_seconds": 60}),
            Some(Duration::from_millis(10)),
        )
        .await
        .unwrap();
    assert_eq!(r["more"], true);
    serde_json::from_value(r["id"].clone()).unwrap()
}

#[tokio::test]
async fn a_wait_that_runs_out_with_no_one_asking_again_goes_ahead_and_so_does_one_from_before_a_restart(
) {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        // The turn was stopped: its plugin never asks again.
        let ask = waiting_ask(&d, s.id, "Which branch?").await;
        assert_eq!(d.line_needs().len(), 1);
        d.0.state.lock().lines.waiting.insert(ask, Instant::now());
        d.line_sweep_waits().await;
        assert_eq!(row(&d, ask).await.asked, Some(Asked::Lapsed));
        assert!(d.line_needs().is_empty());

        // Asked overnight, before the hub restarted: nothing waits on it.
        let mut old = LineMessage::new(
            Party::session(&s),
            Party::you(),
            LineKind::Ask,
            "Keep the old table?".into(),
            1,
        );
        old.at = Utc::now() - chrono::Duration::hours(8);
        old.delivery = Delivery::Delivered;
        d.0.store.put_line(&old).await.unwrap();
        // One asked just now still may be.
        let fresh = waiting_ask(&d, s.id, "Tests too?").await;
        d.line_sweep_waits().await;
        assert_eq!(row(&d, old.id).await.asked, Some(Asked::Lapsed));
        assert_eq!(row(&d, fresh).await.asked, Some(Asked::Open));
        // It went ahead when its longest wait ran out, not when it was found.
        let listed = d.line_went_ahead_list();
        let found = listed
            .iter()
            .find(|w| w.id == format!("line:{}", old.id))
            .unwrap();
        assert!(found.went_ahead_at <= old.at + chrono::Duration::minutes(5));
        // An answered one stays answered.
        d.line_answer(fresh, Sender::Person, "Yes".into())
            .await
            .unwrap();
        d.0.state.lock().lines.waiting.insert(fresh, Instant::now());
        d.line_sweep_waits().await;
        assert_eq!(row(&d, fresh).await.asked, Some(Asked::Answered));
    })
    .await;
}

#[tokio::test]
async fn a_question_is_waited_on_from_when_it_is_told_of() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        // The overseer takes a question up as soon as it hears of it, which
        // may be before the call that asked starts waiting.
        let mut rx = d.0.events.subscribe();
        let asking = {
            let d = d.clone();
            tokio::spawn(async move {
                d.line_call(
                    s.id,
                    "ask",
                    &json!({"to": "you", "text": "May I push?", "wait_seconds": 60}),
                    Some(Duration::from_millis(200)),
                )
                .await
            })
        };
        let ask = loop {
            if let Ok(DeskEvent::Line { message, .. }) = rx.recv().await {
                if message.kind == LineKind::Ask {
                    break message.id;
                }
            }
        };
        assert!(d.waited_on_ask(ask), "waited on as soon as it is heard of");
        asking.await.unwrap().unwrap();
    })
    .await;
}

#[tokio::test]
async fn a_question_given_to_the_person_after_its_session_went_ahead_doesnt_need_them() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        // Asked of the overseer, which didn't get to it before the wait ran
        // out.
        let mut q = LineMessage::new(
            Party::session(&s),
            Party::manager("sarah", "Sarah"),
            LineKind::Ask,
            "May I push?".into(),
            1,
        );
        q.delivery = Delivery::Delivered;
        d.0.store.put_line(&q).await.unwrap();
        let by = By::manager("sarah", "Sarah", "Pushing is the person's.");
        let given = d
            .line_escalate(
                q.id,
                &by,
                line::Escalation {
                    why: "Pushing is the person's.".into(),
                    options: vec![],
                    draft: None,
                },
            )
            .await;
        assert!(matches!(given, Err(LineError::Refused(Refusal::WentAhead))));
        assert_eq!(row(&d, q.id).await.asked, Some(Asked::Lapsed));
        assert!(d.line_needs().is_empty());
        // The overseer's answer still reaches it, later.
        d.line_answer(q.id, Sender::Overseer(by), "Not to main.".into())
            .await
            .unwrap();
        assert_eq!(row(&d, q.id).await.asked, Some(Asked::Answered));
    })
    .await;
}

#[tokio::test]
async fn the_person_sets_a_question_aside_and_nothing_goes_to_the_session() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        // A call waiting on its answer is told to go on at once.
        let asking = {
            let d = d.clone();
            tokio::spawn(async move {
                d.line_call(
                    s.id,
                    "ask",
                    &json!({"to": "you", "text": "Which branch?", "wait_seconds": 60}),
                    Some(Duration::from_secs(20)),
                )
                .await
            })
        };
        until(|| d.line_needs().len() == 1).await;
        let ask: Uuid = d.line_needs()[0].id["line:".len()..].parse().unwrap();
        let set = d.line_dismiss(ask).await.unwrap();
        assert_eq!(set.asked, Some(Asked::Dismissed));
        let result = tokio::time::timeout(Duration::from_secs(5), asking)
            .await
            .expect("woken")
            .unwrap()
            .unwrap();
        assert_eq!(
            result["pending"],
            "The person dismissed this question; carry on with your best judgement and say what you chose."
        );
        assert_eq!(result["more"], false);
        assert!(d.line_needs().is_empty());
        assert!(
            d.0.store
                .line_inbox(&s.id.to_string())
                .await
                .unwrap()
                .is_empty(),
            "nothing sent"
        );
        assert_eq!(row(&d, ask).await.asked, Some(Asked::Dismissed));
        assert!(matches!(
            d.line_dismiss(ask).await,
            Err(LineError::Refused(Refusal::SetAside))
        ));
        assert!(matches!(
            d.line_answer(ask, Sender::Person, "main".into()).await,
            Err(LineError::Refused(Refusal::SetAside))
        ));
        // One the session went ahead on is set aside too; an answered one
        // isn't.
        let r = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "you", "text": "Tests too?", "wait_seconds": 0}),
                None,
            )
            .await
            .unwrap();
        let went: Uuid = serde_json::from_value(r["id"].clone()).unwrap();
        assert_eq!(
            d.line_dismiss(went).await.unwrap().asked,
            Some(Asked::Dismissed)
        );
        let answered = waiting_ask(&d, s.id, "Lint too?").await;
        d.line_answer(answered, Sender::Person, "Yes".into())
            .await
            .unwrap();
        assert!(matches!(
            d.line_dismiss(answered).await,
            Err(LineError::Refused(Refusal::Answered))
        ));
    })
    .await;
}

#[tokio::test]
async fn setting_a_question_aside_over_http_takes_it_off_what_needs_you() {
    use tower::ServiceExt;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        let ask = waiting_ask(&d, s.id, "Which branch?").await;
        let post = |auth| {
            let app = crate::api::sessions_router(d.clone(), auth);
            async move {
                let req = axum::http::Request::post(format!("/api/desk/line/{ask}/dismiss"))
                    .body(axum::body::Body::empty())
                    .unwrap();
                let res = app.oneshot(req).await.unwrap();
                let status = res.status();
                let body = axum::body::to_bytes(res.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (
                    status,
                    serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
                )
            }
        };
        let (status, _) = post(crate::testing::deny_all()).await;
        assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(d.line_needs().len(), 1);
        let (status, body) = post(crate::testing::allow_all()).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(body["asked"], "dismissed");
        assert!(d.line_needs().is_empty());
        let (status, body) = post(crate::testing::allow_all()).await;
        assert_eq!(status, axum::http::StatusCode::CONFLICT);
        assert_eq!(body["error"], "The person set that question aside.");
    })
    .await;
}

#[tokio::test]
async fn what_a_session_goes_ahead_with_is_kept_and_listed_with_what_it_went_ahead_without() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        let r = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "you", "text": "Keep the old table?", "default": " keep it\x1b[2J ", "wait_seconds": 0}),
                None,
            )
            .await
            .unwrap();
        let ask: Uuid = serde_json::from_value(r["id"].clone()).unwrap();
        let q = row(&d, ask).await;
        assert_eq!(q.chose.as_deref(), Some("keep it[2J"), "kept, cleaned");
        assert_eq!(q.asked, Some(Asked::Lapsed));
        let went = d.line_went_ahead_list();
        assert_eq!(went.len(), 1);
        assert_eq!(went[0].id, format!("line:{ask}"));
        assert_eq!(went[0].ask, "Keep the old table?");
        assert_eq!(went[0].chose.as_deref(), Some("keep it[2J"));
        assert_eq!(went[0].who.id, s.id.to_string());
        let json = serde_json::to_value(&went[0]).unwrap();
        assert!(json.get("askedAt").is_some() && json.get("wentAheadAt").is_some());
        d.line_dismiss(ask).await.unwrap();
        assert!(d.line_went_ahead_list().is_empty(), "dismissed: gone");
    })
    .await;
}

#[tokio::test]
async fn an_ask_in_parts_is_kept_and_listed_and_a_long_question_is_sent_back() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        // A question longer than a line isn't sent: the session reads how to
        // ask it in parts.
        let refused = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "you", "question": "Should PR 2 include the stub service?\nThe design lists it, the brief doesn't.", "wait_seconds": 0}),
                None,
            )
            .await
            .unwrap_err();
        assert!(refused.contains("move the rest"), "{refused}");
        assert!(d.line_went_ahead_list().is_empty(), "nothing was sent");
        let r = d
            .line_call(
                s.id,
                "ask",
                &json!({
                    "to": "you",
                    "question": "Should PR 2 include the stub service?",
                    "default": "include it",
                    "why": "The design lists it.",
                    "details": "- the stub\n- its `api_url`\x1b[2J",
                    "about": "W7 · PR 2",
                    "wait_seconds": 0,
                }),
                None,
            )
            .await
            .unwrap();
        let ask: Uuid = serde_json::from_value(r["id"].clone()).unwrap();
        let q = row(&d, ask).await;
        assert_eq!(q.text, "Should PR 2 include the stub service?");
        assert_eq!(q.about.as_deref(), Some("W7 · PR 2"));
        assert_eq!(q.why.as_deref(), Some("The design lists it."));
        assert_eq!(q.details.as_deref(), Some("- the stub\n- its `api_url`[2J"), "kept, cleaned");
        let json = serde_json::to_value(&q).unwrap();
        assert_eq!(json["question"]["headline"], "Should PR 2 include the stub service?");
        assert_eq!(json["question"]["details"], "- the stub\n- its `api_url`[2J");
        let went = d.line_went_ahead_list();
        assert_eq!(went[0].ask, "Should PR 2 include the stub service?");
        assert_eq!(went[0].chose.as_deref(), Some("include it"));
    })
    .await;
}

#[tokio::test]
async fn a_question_stored_in_one_block_reads_as_a_headline_and_details() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        let wall = "Should the import keep the old table (see `docs/import.md`, step 2)? It still has rows from March. Default if no answer: keep it.";
        let mut m = LineMessage::new(
            Party::session(&d.view(s.id).unwrap()),
            Party::you(),
            LineKind::Ask,
            wall.into(),
            1,
        );
        m.asked = Some(Asked::Lapsed);
        d.0.store.put_line(&m).await.unwrap();
        let q = row(&d, m.id).await;
        assert_eq!(q.text, wall, "stored as it was said");
        assert_eq!(q.details, None);
        let shown = q.question.clone().unwrap();
        assert_eq!(shown.headline, "Should the import keep the old table (see `docs/import.md`, step 2)?");
        assert_eq!(shown.details.as_deref(), Some("It still has rows from March. Default if no answer: keep it."));
        // Written again (as a dismiss does), what is stored doesn't change.
        d.0.store.put_line(&q).await.unwrap();
        let again = row(&d, m.id).await;
        assert_eq!((again.text.as_str(), again.details.as_deref()), (wall, None));
    })
    .await;
}

#[tokio::test]
async fn the_persons_draft_goes_in_its_prompt_unsent_and_holds_what_desk_would_type_in() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        assert!(!d.typed_since_idle(s.id));
        d.line_draft(s.id, "About your questions:\n- one\r\x1b[201~\r")
            .unwrap();
        assert!(d.typed_since_idle(s.id), "the pump waits for them");
        assert!(matches!(
            d.line_draft(s.id, "  "),
            Err(DeskError::Invalid(_))
        ));
        d.change(s.id, |r, _| r.state = SessionState::Blocked);
        assert!(matches!(
            d.line_draft(s.id, "About your questions:"),
            Err(DeskError::Conflict(_))
        ));
    })
    .await;
}

#[test]
fn a_draft_is_one_paste_with_no_enter() {
    let bytes = draft_bytes("About it:\n- one\r\x1b[201~\r/clear\r");
    assert_eq!(
        bytes,
        b"\x1b[200~About it:\n- one[201~/clear\x1b[201~".to_vec()
    );
    assert!(!bytes.contains(&b'\r'));
}

#[test]
fn a_line_kind_reads_back() {
    for k in [LineKind::Ask, LineKind::Tell, LineKind::Answer] {
        assert_eq!(LineKind::parse(k.as_str()).unwrap(), k);
    }
    for a in [Asked::Lapsed, Asked::Dismissed] {
        assert_eq!(Asked::parse(a.as_str()).unwrap(), a);
    }
    assert_eq!(serde_json::to_value(Asked::Lapsed).unwrap(), "lapsed");
}

#[tokio::test]
async fn a_reply_names_its_question_as_the_session_read_it_mark_or_id() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let a = new_session(&d, dir.path()).await;
        let b = new_session(&d, dir.path()).await;
        idle(&d, a.id).await;
        idle(&d, b.id).await;
        // A asks B, not waiting: B answers later on its own line.
        let ask = |text: &'static str| {
            let d = d.clone();
            async move {
                let r = d
                    .line_call(
                        a.id,
                        "ask",
                        &json!({"to": b.id, "text": text, "wait_seconds": 0}),
                        None,
                    )
                    .await
                    .unwrap();
                serde_json::from_value::<Uuid>(r["id"].clone()).unwrap()
            }
        };
        let mark_of = |id: Uuid| id.simple().to_string()[..8].to_string();
        for given in [
            |m: &str, _: Uuid| m.to_string(),
            |m: &str, _: Uuid| format!("m:{m}"),
            |m: &str, _: Uuid| format!("[Desk m:{m}]"),
            |_: &str, id: Uuid| id.to_string(),
        ] {
            let q = ask("Which port?").await;
            let r = d
                .line_call(
                    b.id,
                    "reply",
                    &json!({"message_id": given(&mark_of(q), q), "text": "5433"}),
                    None,
                )
                .await
                .unwrap();
            let answer: Uuid = serde_json::from_value(r["id"].clone()).unwrap();
            assert_eq!(row(&d, answer).await.in_reply_to, Some(q));
            assert_eq!(row(&d, q).await.asked, Some(Asked::Answered));
        }
        // A mark of no question to this session is said so, plainly.
        let q = ask("Tests too?").await;
        let refused = d
            .line_call(
                a.id,
                "reply",
                &json!({"message_id": mark_of(q), "text": "yes"}),
                None,
            )
            .await
            .unwrap_err();
        assert!(refused.contains("No question to you"), "{refused}");
    })
    .await;
}

/// A move on a row is one statement that checks where the row is: what
/// moved it first stands, and is not written over by a copy read before.
#[tokio::test]
async fn a_row_that_moved_on_is_not_written_over_by_a_move_decided_from_an_older_read() {
    with_test_pool(build_pool, |pool| async move {
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool);
        let session = Uuid::new_v4();
        let sess = |id: Uuid| Party {
            kind: PartyKind::Session,
            id: id.to_string(),
            name: "scout".into(),
            engine: None,
            origin: None,
        };
        let ask = |to: Party| {
            LineMessage::new(sess(session), to, LineKind::Ask, "Which branch?".into(), 0)
        };

        // A question dismissed after it was read stays dismissed.
        let q = ask(Party::you());
        store.put_line(&q).await.unwrap();
        store.line_dismiss(q.id).await.unwrap().unwrap();
        assert!(store
            .line_withdraw_ask(q.id, &[Asked::Open, Asked::Escalated, Asked::Lapsed])
            .await
            .unwrap()
            .is_none());
        assert!(store
            .line_escalate_ask(
                q.id,
                &line::Escalation {
                    why: String::new(),
                    options: vec![],
                    draft: None,
                }
            )
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            store.line_message(q.id).await.unwrap().unwrap().asked,
            Some(Asked::Dismissed)
        );

        // An open one is withdrawn, once.
        let q = ask(Party::you());
        store.put_line(&q).await.unwrap();
        let gone = store
            .line_withdraw_ask(q.id, &[Asked::Open])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(gone.asked, Some(Asked::Withdrawn));
        assert!(store
            .line_withdraw_ask(q.id, &[Asked::Open])
            .await
            .unwrap()
            .is_none());

        // A row taken is not put back to waiting, nor held, by a decision
        // made before it was taken; and goes back only to its own session.
        let mut m = LineMessage::new(
            Party::desk(),
            sess(session),
            LineKind::Tell,
            "hello".into(),
            0,
        );
        store.put_line(&m).await.unwrap();
        store
            .line_take(m.id, Delivery::Delivered)
            .await
            .unwrap()
            .unwrap();
        assert!(store
            .line_set_waiting(m.id, Delivery::Held)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .line_untake(m.id, &Uuid::new_v4().to_string())
            .await
            .unwrap()
            .is_none());
        let back = store
            .line_untake(m.id, &session.to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((back.delivery, back.delivered_at), (Delivery::Queued, None));
        m = store
            .line_set_waiting(m.id, Delivery::Held)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.delivery, Delivery::Held);
    })
    .await;
}

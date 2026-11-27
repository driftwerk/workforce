//! A Codex session's requests, answered from Desk: against a stand-in
//! app-server that speaks the protocol as CLI 0.166 does.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio_tungstenite::WebSocketStream;
use tokio_util::sync::CancellationToken;
use workforce_postgres::test_support::with_test_pool;

use super::tests::{logs, options, Logged};
use super::{Desk, DeskError, Floor};
use crate::codex::tests::{next_sent, say};
use crate::crew::By;
use crate::floor::{Answer, DecideRef, TurnOp};
use crate::limit::TurnFailure;
use crate::model::{Decision, Engine, NewSession, SessionState};
use crate::store::{build_pool, DeskStore};

/// A stand-in `codex`: as `app-server --listen unix://…` it says where it
/// was told to listen, for the test to listen there; as the TUI it waits.
fn fake_codex(dir: &Path) -> PathBuf {
    let path = dir.join("fake-codex");
    workforce_common::executable::write(
        &path,
        format!(
            "#!/bin/sh\ncase \"$1\" in app-server|--remote|resume) ;; *) exit 0 ;; esac\n\
             [ \"$1\" = app-server ] && echo \"${{3#unix://}}\" > '{d}/listen'\n\
             case \"$1\" in --remote|resume) echo \"$@\" > '{d}/tui-args' ;; esac\nexec sleep 60\n",
            d = dir.display()
        ),
    )
    .unwrap();
    path
}

/// Where the stand-in in `dir` was told to listen, once it was.
async fn listened(dir: &Path) -> PathBuf {
    let told = dir.join("listen");
    loop {
        if let Ok(s) = std::fs::read_to_string(&told) {
            if !s.trim().is_empty() {
                return PathBuf::from(s.trim());
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Listens where the stand-in was told to, and takes the observer's
/// connection.
fn serve(dir: &Path) -> tokio::task::JoinHandle<WebSocketStream<UnixStream>> {
    let dir = dir.to_path_buf();
    tokio::spawn(async move {
        let listener = tokio::net::UnixListener::bind(listened(&dir).await).unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        tokio_tungstenite::accept_async(stream).await.unwrap()
    })
}

/// Listens where the stand-in was told to, and takes each connection the
/// observer makes; once the receiver is dropped, each is closed at once.
fn serving(dir: &Path) -> tokio::sync::mpsc::UnboundedReceiver<WebSocketStream<UnixStream>> {
    let dir = dir.to_path_buf();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut each = crate::codex::tests::fake_servers(&listened(&dir).await);
        while let Some(ws) = each.recv().await {
            if tx.send(ws).is_err() {
                return;
            }
        }
    });
    rx
}

pub(super) async fn until(mut ok: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("not in time");
}

fn ask(id: i64, decisions: Option<Value>) -> Value {
    let mut params = json!({"threadId": "t1", "turnId": "u1", "itemId": format!("exec-{id}"),
        "startedAtMs": 1, "reason": "Delete it?", "command": "/bin/zsh -lc 'rm a.txt'",
        "cwd": "/repo", "commandActions": [{"type": "unknown", "command": "rm a.txt"}]});
    if let Some(d) = decisions {
        params["availableDecisions"] = d;
    }
    json!({"id": id, "method": "item/commandExecution/requestApproval", "params": params})
}

pub(super) fn codex_session(dir: &Path, approve_for_me: bool) -> NewSession {
    NewSession {
        fresh_worktree: false,
        engine: Engine::Codex,
        view: None,
        name: "cleanup".into(),
        cwd: dir.to_string_lossy().into_owned(),
        goal: String::new(),
        worktree: None,
        prompt: None,
        approvals: None,
        approve_for_me,
        fast: false,
        model: None,
        account: None,
        env: Default::default(),
        group: None,
        review_of: None,
    }
}

fn status(flags: &[&str]) -> Value {
    json!({"method": "thread/status/changed",
        "params": {"threadId": "t1", "status": {"type": "active", "activeFlags": flags}}})
}

#[tokio::test]
async fn codex_asks_are_answered_from_desk_and_go_when_answered_anywhere() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let logged = Arc::new(Logged::default());
        let d = Desk::start(
            DeskStore::from_pool(pool),
            options(dir.path()).with_program(Engine::Codex, fake_codex(dir.path())),
            Floor {
                turns: Some(logged.clone()),
                plans: None,
                pricer: None,
                redactor: None,
                index: None,
                deriver: None,
                accountant: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let server = serve(dir.path());
        let s = d
            .create(codex_session(dir.path(), false), None)
            .await
            .unwrap();
        let mut ws = server.await.unwrap();
        next_sent(&mut ws, |v| v["method"] == "initialized").await;
        say(&mut ws, json!({"method": "thread/started", "params": {"thread": {"id": "t1",
            "environments": [{"cwd": "/repo"}], "path": null}}})).await;
        say(&mut ws, status(&[])).await;
        let resume = next_sent(&mut ws, |v| v["method"] == "thread/resume").await;
        assert_eq!(resume["params"]["threadId"], "t1", "subscribed, to be asked");
        say(&mut ws, json!({"id": resume["id"], "result": {"thread": {"id": "t1"}}})).await;

        // It says it waits, then what for: the request, answerable.
        say(&mut ws, status(&["waitingOnApproval"])).await;
        until(|| d.view(s.id).unwrap().state == SessionState::Blocked).await;
        say(&mut ws, ask(0, Some(json!(["accept", {"acceptWithExecpolicyAmendment":
            {"execpolicy_amendment": ["rm", "a.txt"]}}, "cancel"])))).await;
        until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.answerable)).await;
        let v = d.view(s.id).unwrap();
        let p = v.pending.unwrap();
        assert_eq!(v.state, SessionState::Blocked);
        assert_eq!((p.tool.as_str(), p.detail.as_str()), ("Bash", "rm a.txt"));
        assert_eq!(p.choices, [Decision::Allow, Decision::Cancel]);
        assert!(
            logs(&logged, |ops| ops.iter().any(|op| matches!(op,
                TurnOp::Wait { wait, .. } if wait.key == p.id.to_string()
                    && matches!(&wait.answer, Answer::Decide { at: DecideRef::Codex { choices, .. } }
                        if *choices == [Decision::Allow, Decision::Cancel])
                    && wait.ask.starts_with("Run rm a.txt?")
                    && wait.ask.contains("Delete it?"))))
            .await,
            "the Floor's need carries what Codex offers"
        );
        let you = || Some(By::you());
        // Acting fully anywhere but not unbound, a boss still never answers
        // Codex.
        d.set_full(std::collections::HashMap::from([(
            "crew".to_string(),
            crate::charge::FullGrant {
                anywhere: true,
                ..Default::default()
            },
        )]));
        for agent in [None, Some(By::manager("crew", "Crew", "it only reads"))] {
            assert!(
                matches!(d.decide(s.id, p.id, Decision::Allow, agent), Err(DeskError::Invalid(_))),
                "an agent's tool never lets Codex out of its sandbox"
            );
        }
        assert!(
            matches!(d.decide(s.id, p.id, Decision::Deny, you()), Err(DeskError::Invalid(_))),
            "Codex didn't offer it"
        );
        // Unbound, its boss decides it: a no, where Codex offers only to
        // cancel, cancels it.
        d.set_full(std::collections::HashMap::from([(
            "crew".to_string(),
            crate::charge::FullGrant {
                anywhere: true,
                unbound: true,
                ..Default::default()
            },
        )]));
        d.decide(
            s.id,
            p.id,
            Decision::Deny,
            Some(By::manager("crew", "Crew", "it deletes a file the task needs")),
        )
        .unwrap();
        let answer = next_sent(&mut ws, |v| v.get("result").is_some()).await;
        assert_eq!(answer, json!({"id": 0, "result": {"decision": "cancel"}}));
        assert!(d.view(s.id).unwrap().pending.is_none());
        assert!(
            matches!(d.decide(s.id, p.id, Decision::Allow, you()), Err(DeskError::Conflict(_))),
            "answered once"
        );
        say(&mut ws, json!({"method": "serverRequest/resolved",
            "params": {"threadId": "t1", "requestId": 0}})).await;

        // What Desk didn't see whole is the person's to allow: a command it
        // only has Codex's reading of, and one too long to show.
        d.boss_bounds_with(crate::charge::Bounds {
            home: dir.path().to_path_buf(),
            ..Default::default()
        });
        let mut blind = ask(10, None);
        blind["params"].as_object_mut().unwrap().remove("command");
        let mut long = ask(11, None);
        long["params"]["command"] = json!("x".repeat(9000));
        for (n, request) in [(10, blind), (11, long)] {
            say(&mut ws, request).await;
            until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.answerable)).await;
            let p = d.view(s.id).unwrap().pending.unwrap();
            assert!(
                matches!(
                    d.decide(s.id, p.id, Decision::Allow, Some(By::manager("crew", "Crew", "fine"))),
                    Err(DeskError::Invalid(m)) if m.contains("didn't see all")
                ),
                "request {n}"
            );
            d.decide(s.id, p.id, Decision::Cancel, you()).unwrap();
            next_sent(&mut ws, |v| v["id"] == n).await;
            say(&mut ws, json!({"method": "serverRequest/resolved",
                "params": {"threadId": "t1", "requestId": n}})).await;
            until(|| d.view(s.id).unwrap().pending.is_none()).await;
        }

        // Answered in the terminal: it goes from Desk too.
        say(&mut ws, ask(1, None)).await;
        until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.answerable)).await;
        assert_eq!(d.view(s.id).unwrap().pending.unwrap().choices.len(), 4, "unlisted: all four");
        say(&mut ws, json!({"method": "serverRequest/resolved",
            "params": {"threadId": "t1", "requestId": 1}})).await;
        until(|| d.view(s.id).unwrap().pending.is_none()).await;
        assert_eq!(d.view(s.id).unwrap().state, SessionState::Working);

        // Two at once: each goes when its own is answered.
        say(&mut ws, ask(3, None)).await;
        until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.answerable)).await;
        let first = d.view(s.id).unwrap().pending.unwrap();
        say(&mut ws, ask(4, None)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(d.view(s.id).unwrap().pending.unwrap().id, first.id, "the oldest first");
        say(&mut ws, json!({"method": "serverRequest/resolved",
            "params": {"threadId": "t1", "requestId": 4}})).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(d.view(s.id).unwrap().pending.unwrap().id, first.id, "not the other's answer");
        d.decide(s.id, first.id, Decision::Allow, you()).unwrap();
        assert_eq!(
            next_sent(&mut ws, |v| v.get("result").is_some()).await,
            json!({"id": 3, "result": {"decision": "accept"}})
        );
        assert!(d.view(s.id).unwrap().pending.is_none(), "none left");
        say(&mut ws, json!({"method": "serverRequest/resolved",
            "params": {"threadId": "t1", "requestId": 3}})).await;

        // Its server goes while it asks: so does the request.
        say(&mut ws, ask(2, None)).await;
        until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.answerable)).await;
        drop(ws);
        until(|| d.view(s.id).unwrap().pending.is_none()).await;
    })
    .await;
}

#[tokio::test]
async fn approve_for_me_starts_codex_with_its_reviewer_and_stays_on() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let d = Desk::start(
            DeskStore::from_pool(pool),
            options(dir.path()).with_program(Engine::Codex, fake_codex(dir.path())),
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let claude = NewSession {
            engine: Engine::Claude,
            ..codex_session(dir.path(), true)
        };
        assert!(
            matches!(d.create(claude, None).await, Err(DeskError::Invalid(_))),
            "Codex's alone"
        );
        let server = serve(dir.path());
        let s = d.create(codex_session(dir.path(), true), None).await.unwrap();
        let _ws = server.await.unwrap();
        assert!(s.approve_for_me, "the session says it is on");
        let args = dir.path().join("tui-args");
        until(|| std::fs::read_to_string(&args).is_ok_and(|a| a.ends_with('\n'))).await;
        let args = std::fs::read_to_string(&args).unwrap();
        assert!(
            args.contains("-c approvals_reviewer=auto_review -c approval_policy=on-request -c sandbox_mode=workspace-write"),
            "{args}"
        );
        // The row is queued as the session starts: written once the queue is.
        d.persisted().await;
        assert!(store.get(s.id).await.unwrap().unwrap().approve_for_me, "kept, for its resume");
    })
    .await;
}

#[tokio::test]
async fn fast_mode_starts_codex_on_the_priority_tier_and_stays_on() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let d = Desk::start(
            DeskStore::from_pool(pool),
            options(dir.path()).with_program(Engine::Codex, fake_codex(dir.path())),
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let claude = NewSession {
            engine: Engine::Claude,
            fast: true,
            model: None,
            ..codex_session(dir.path(), false)
        };
        assert!(
            matches!(d.create(claude, None).await, Err(DeskError::Invalid(_))),
            "Codex's alone"
        );
        let server = serve(dir.path());
        let fast = NewSession {
            fast: true,
            model: None,
            ..codex_session(dir.path(), false)
        };
        let s = d.create(fast, None).await.unwrap();
        let _ws = server.await.unwrap();
        assert!(s.fast, "the session says it is on");
        assert!(s.caps.fast, "Codex can");
        let args = dir.path().join("tui-args");
        until(|| std::fs::read_to_string(&args).is_ok_and(|a| a.ends_with('\n'))).await;
        let args = std::fs::read_to_string(&args).unwrap();
        assert!(args.contains("-c service_tier=\"priority\""), "{args}");
        // The row is queued as the session starts: written once the queue is.
        d.persisted().await;
        assert!(
            store.get(s.id).await.unwrap().is_some_and(|r| r.fast),
            "kept, for its resume"
        );
    })
    .await;
}

/// What Codex asks in its terminal (`item/tool/requestUserInput`, as
/// 0.166's app-server schema has it) shows with its words and options,
/// is never answered from Desk, and goes once it is answered.
#[tokio::test]
async fn a_codex_question_shows_its_words_until_the_terminal_answers_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let logged = Arc::new(Logged::default());
        let d = Desk::start(
            DeskStore::from_pool(pool),
            options(dir.path()).with_program(Engine::Codex, fake_codex(dir.path())),
            Floor {
                turns: Some(logged.clone()),
                plans: None,
                pricer: None,
                redactor: None,
                index: None,
                deriver: None,
                accountant: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let server = serve(dir.path());
        let s = d
            .create(codex_session(dir.path(), false), None)
            .await
            .unwrap();
        let mut ws = server.await.unwrap();
        next_sent(&mut ws, |v| v["method"] == "initialized").await;
        say(&mut ws, json!({"method": "thread/started", "params": {"thread": {"id": "t1",
            "environments": [{"cwd": "/repo"}], "path": null}}})).await;
        say(&mut ws, status(&[])).await;
        let resume = next_sent(&mut ws, |v| v["method"] == "thread/resume").await;
        say(&mut ws, json!({"id": resume["id"], "result": {"thread": {"id": "t1"}}})).await;

        // Its status says it waits on the person, then the request says what for.
        say(&mut ws, status(&["waitingOnUserInput"])).await;
        until(|| d.view(s.id).unwrap().state == SessionState::Blocked).await;
        assert_eq!(
            d.view(s.id).unwrap().pending,
            None,
            "nothing yet says what it waits on"
        );
        say(&mut ws, json!({"id": 4, "method": "item/tool/requestUserInput", "params": {
            "threadId": "t1", "turnId": "u1", "itemId": "q-1", "isBlocking": true,
            "questions": [{"id": "where", "header": "Target", "question": "Deploy main or the release branch?",
                "isOther": true, "isSecret": false, "options": [
                    {"label": "main", "description": "What is on main now."},
                    {"label": "release", "description": "The release branch."}]}]}})).await;
        until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.tool == "Question")).await;
        let p = d.view(s.id).unwrap().pending.unwrap();
        assert_eq!(
            (p.detail.as_str(), p.options.clone(), p.answerable),
            ("Deploy main or the release branch?", Some(vec!["main".to_string(), "release".to_string()]), false)
        );
        assert!(d.decide(s.id, p.id, Decision::Allow, None).is_err(), "the terminal's to answer");
        assert!(
            logs(&logged, |ops| ops.iter().any(|op| matches!(op, TurnOp::Wait { wait, .. }
                if wait.options.is_some() && wait.answer == Answer::Terminal)))
            .await
        );
        // A question the turn goes on past doesn't wait.
        say(&mut ws, json!({"id": 5, "method": "item/tool/requestUserInput", "params": {
            "threadId": "t1", "turnId": "u1", "itemId": "q-2", "isBlocking": false,
            "questions": [{"id": "x", "header": "Later", "question": "Anything else?"}]}})).await;
        // Answered in the terminal.
        say(&mut ws, json!({"method": "serverRequest/resolved", "params": {"threadId": "t1", "requestId": 4}})).await;
        until(|| d.view(s.id).unwrap().pending.is_none()).await;
        assert_eq!(d.view(s.id).unwrap().state, SessionState::Working);
        say(&mut ws, status(&[])).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let v = d.view(s.id).unwrap();
        assert_eq!((v.state, v.pending), (SessionState::Working, None));

        // Its boss answers its questions: every one, by its id, in the
        // person's place.
        d.set_full(std::collections::HashMap::from([(
            "crew".to_string(),
            crate::charge::FullGrant {
                anywhere: true,
                unbound: true,
                ..Default::default()
            },
        )]));
        d.set_dialog_answerers(std::collections::HashMap::from([(
            s.id,
            super::DialogAnswerer {
                boss: "crew".into(),
                gap: chrono::Duration::zero(),
            },
        )]));
        say(&mut ws, json!({"id": 6, "method": "item/tool/requestUserInput", "params": {
            "threadId": "t1", "turnId": "u2", "itemId": "q-3", "isBlocking": true,
            "questions": [
                {"id": "where", "header": "Target", "question": "Deploy main or the release branch?",
                 "isOther": false, "isSecret": false, "options": [
                    {"label": "main", "description": "What is on main now."},
                    {"label": "release", "description": "The release branch."}]},
                {"id": "note", "header": "Note", "question": "What should the release note say?",
                 "isOther": true, "isSecret": false, "options": null}]}})).await;
        until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.answerable)).await;
        let p = d.view(s.id).unwrap().pending.unwrap();
        assert_eq!(p.request.as_ref().unwrap().input["questions"][1]["id"], "note");
        let boss = |said: &str| Some(By::manager("crew", "Crew", said));
        assert!(
            matches!(d.decide(s.id, p.id, Decision::Deny, boss(r#"{"where": "staging"}"#)), Err(DeskError::Invalid(_))),
            "not one of its options, and the note unanswered"
        );
        assert!(d.view(s.id).unwrap().pending.is_some_and(|q| q.id == p.id && q.answerable));
        d.decide(
            s.id,
            p.id,
            Decision::Deny,
            boss(r#"{"where": "Release", "note": "Fixes the invite emails."}"#),
        )
        .unwrap();
        assert_eq!(
            next_sent(&mut ws, |v| v.get("result").is_some()).await,
            json!({"id": 6, "result": {"answers": {
                "where": {"answers": ["release"]},
                "note": {"answers": ["Fixes the invite emails."]}}}})
        );
        assert!(d.view(s.id).unwrap().pending.is_none(), "answered");
        say(&mut ws, json!({"method": "serverRequest/resolved", "params": {"threadId": "t1", "requestId": 6}})).await;

        // A secret is the person's to type, always.
        say(&mut ws, json!({"id": 7, "method": "item/tool/requestUserInput", "params": {
            "threadId": "t1", "turnId": "u2", "itemId": "q-4", "isBlocking": true,
            "questions": [{"id": "key", "header": "Key", "question": "Paste the API key",
                "isOther": true, "isSecret": true, "options": null}]}})).await;
        until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.detail == "Paste the API key")).await;
        let secret = d.view(s.id).unwrap().pending.unwrap();
        assert!(!secret.answerable && secret.request.is_none());
        assert!(d.decide(s.id, secret.id, Decision::Deny, boss(r#"{"key": "sk-x"}"#)).is_err());

        // The person writing to it takes a held question back: its terminal asks.
        say(&mut ws, json!({"method": "serverRequest/resolved", "params": {"threadId": "t1", "requestId": 7}})).await;
        until(|| d.view(s.id).unwrap().pending.is_none()).await;
        say(&mut ws, json!({"id": 8, "method": "item/tool/requestUserInput", "params": {
            "threadId": "t1", "turnId": "u2", "itemId": "q-5", "isBlocking": true,
            "questions": [{"id": "go", "header": "Go", "question": "Go on?", "isOther": true}]}})).await;
        until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.answerable)).await;
        let held = d.view(s.id).unwrap().pending.unwrap();
        d.person_typing(s.id).unwrap();
        assert!(d.view(s.id).unwrap().pending.is_some_and(|q| q.id == held.id && !q.answerable));
        assert!(d.decide(s.id, held.id, Decision::Deny, boss(r#"{"go": "yes"}"#)).is_err());
    })
    .await;
}

/// A Codex session an agent starts (by a plan, a handoff or the crew) runs
/// in Fast mode when the agent does; the person and Desk choose per session.
#[tokio::test]
async fn an_agents_codex_sessions_run_as_fast_as_the_agent() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let d = Desk::start(
            DeskStore::from_pool(pool),
            // The stand-in: Desk reads no real `codex` it may find.
            options(dir.path()).with_program(Engine::Codex, fake_codex(dir.path())),
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let boss = crate::crew::Party::manager("boss-1", "Boss");
        assert!(!d.fast_for(Engine::Codex, &boss), "until the hub says");
        d.agent_fast_with(Arc::new(|agent: &str| agent == "boss-1"));
        assert!(d.fast_for(Engine::Codex, &boss));
        assert!(!d.fast_for(Engine::Claude, &boss), "Codex alone");
        assert!(!d.fast_for(
            Engine::Codex,
            &crate::crew::Party::manager("boss-2", "Other")
        ));
        assert!(!d.fast_for(Engine::Codex, &crate::crew::Party::you()));
    })
    .await;
}

/// A turn that hits the plan's usage limit, as Codex 0.166 recorded it:
/// its thread goes to `systemError`, never `idle`, and the session is done
/// with it rather than working for good.
#[tokio::test]
async fn a_codex_turn_that_failed_leaves_its_session_done() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let d = Desk::start(
            DeskStore::from_pool(pool),
            options(dir.path()).with_program(Engine::Codex, fake_codex(dir.path())),
            Floor {
                turns: None,
                plans: None,
                pricer: None,
                redactor: None,
                index: None,
                deriver: None,
                accountant: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let server = serve(dir.path());
        let s = d
            .create(codex_session(dir.path(), false), None)
            .await
            .unwrap();
        let mut ws = server.await.unwrap();
        next_sent(&mut ws, |v| v["method"] == "initialized").await;
        let recording = include_str!("../../fixtures/codex/app-server-0.166-failed.jsonl");
        let lines: Vec<Value> = recording
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let (turn, end) = lines.split_at(lines.len() - 3);
        for v in turn {
            say(&mut ws, v.clone()).await;
        }
        until(|| d.view(s.id).unwrap().state == SessionState::Working).await;
        assert_eq!(
            d.view(s.id).unwrap().last_prompt.as_deref(),
            Some("fix the failing test")
        );
        // `systemError`, the error, and the turn's completion as failed.
        for v in end {
            say(&mut ws, v.clone()).await;
        }
        until(|| d.view(s.id).unwrap().state == SessionState::Done).await;
        // How it failed, once Codex says: on its plan's usage limit.
        until(|| {
            d.view(s.id)
                .unwrap()
                .failure
                .as_ref()
                .is_some_and(|f| f.limit().is_some())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let v = d.view(s.id).unwrap();
        assert_eq!(v.state, SessionState::Done, "and stays so");
        assert_eq!(
            serde_json::to_value(&v).unwrap()["failed"],
            "It stopped on its plan's usage limit"
        );
        // Asked again: its failure is behind it.
        say(
            &mut ws,
            json!({"method": "turn/started", "params": {"threadId": "t1", "turn": {"id": "u2"}}}),
        )
        .await;
        until(|| d.view(s.id).unwrap().failure.is_none()).await;
    })
    .await;
}

/// A turn whose completion says it failed before its status does is done,
/// with Codex's words, and asks nothing.
#[tokio::test]
async fn a_codex_turn_completed_failed_first_is_done_with_its_words() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, s, mut ws) = following(pool, dir.path()).await;
        say(
            &mut ws,
            json!({"method": "turn/started", "params": {"threadId": "t1", "turn": {"id": "u1"}}}),
        )
        .await;
        say(
            &mut ws,
            json!({"method": "thread/status/changed", "params": {"threadId": "t1",
            "status": {"type": "active", "activeFlags": []}}}),
        )
        .await;
        until(|| d.view(s.id).unwrap().state == SessionState::Working).await;
        say(
            &mut ws,
            json!({"method": "turn/completed", "params": {"threadId": "t1", "turn": {"id": "u1",
            "status": "failed", "error": {"message": "stream disconnected before completion"}}}}),
        )
        .await;
        until(|| d.view(s.id).unwrap().state == SessionState::Done).await;
        let v = d.view(s.id).unwrap();
        assert_eq!(
            v.failure,
            Some(TurnFailure::Failed(
                "stream disconnected before completion".into()
            ))
        );
        assert_eq!(v.asked, None);
        say(
            &mut ws,
            json!({"method": "thread/status/changed", "params": {"threadId": "t1",
            "status": {"type": "systemError"}}}),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(d.view(s.id).unwrap().failure, v.failure, "its words kept");
    })
    .await;
}

/// The observer's connection to the app-server lost while its TUI runs:
/// followed again, what Codex still asks stays the same request, answered
/// from Desk there; not followed again in time, the session says Desk
/// lost sight of it, said in Activity too, until it is followed again.
#[tokio::test]
async fn a_codex_session_followed_again_keeps_its_request_and_one_lost_says_so() {
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
        let mut servers = serving(dir.path());
        let s = d
            .create(codex_session(dir.path(), false), None)
            .await
            .unwrap();
        let mut ws = servers.recv().await.unwrap();
        next_sent(&mut ws, |v| v["method"] == "initialized").await;
        say(
            &mut ws,
            json!({"method": "thread/started", "params": {"thread": {"id": "t1",
            "environments": [{"cwd": "/repo"}], "path": null}}}),
        )
        .await;
        say(&mut ws, status(&[])).await;
        let resume = next_sent(&mut ws, |v| v["method"] == "thread/resume").await;
        say(&mut ws, json!({"id": resume["id"], "result": {"thread": {"id": "t1"}}})).await;
        say(&mut ws, status(&["waitingOnApproval"])).await;
        say(&mut ws, ask(0, None)).await;
        until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.answerable)).await;
        let p = d.view(s.id).unwrap().pending.unwrap();

        drop(ws);
        let mut ws = servers.recv().await.unwrap();
        let resume = next_sent(&mut ws, |v| v["method"] == "thread/resume").await;
        assert_eq!(resume["params"]["threadId"], "t1");
        say(
            &mut ws,
            json!({"id": resume["id"], "result": {"thread": {"id": "t1",
            "status": {"type": "active", "activeFlags": ["waitingOnApproval"]}}}}),
        )
        .await;
        say(&mut ws, ask(0, None)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let v = d.view(s.id).unwrap();
        assert_eq!(v.state, SessionState::Blocked);
        assert_eq!(v.pending.as_ref().map(|q| q.id), Some(p.id), "the same request");
        assert_eq!(v.failure, None);
        d.decide(s.id, p.id, Decision::Allow, Some(By::you()))
            .unwrap();
        let answer = next_sent(&mut ws, |v| v.get("result").is_some()).await;
        assert_eq!(answer, json!({"id": 0, "result": {"decision": "accept"}}));
        say(
            &mut ws,
            json!({"method": "serverRequest/resolved", "params": {"threadId": "t1", "requestId": 0}}),
        )
        .await;
        say(&mut ws, ask(1, None)).await;
        until(|| d.view(s.id).unwrap().pending.is_some_and(|q| q.id != p.id)).await;

        // The app-server takes no more connections that last.
        drop(ws);
        let lost = Some(TurnFailure::Failed(
            "Desk lost sight of this Codex session".into(),
        ));
        for _ in 0..100 {
            if d.view(s.id).unwrap().failure == lost {
                break;
            }
            if let Ok(ws) = servers.try_recv() {
                drop(ws);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let v = d.view(s.id).unwrap();
        assert_eq!(v.failure, lost);
        assert_eq!((v.state, v.pending), (SessionState::Done, None));
        let said = d
            .activity(crate::crew::Filter::All, None, 50)
            .await
            .unwrap()
            .events
            .iter()
            .any(|e| e.text.contains("Desk lost sight"));
        assert!(said, "said in Activity");

        // Its app-server back: followed again, what it says is so again.
        let mut ws = servers.recv().await.unwrap();
        let resume = next_sent(&mut ws, |v| v["method"] == "thread/resume").await;
        say(
            &mut ws,
            json!({"id": resume["id"], "result": {"thread": {"id": "t1",
            "status": {"type": "active", "activeFlags": ["waitingOnApproval"]}}}}),
        )
        .await;
        say(&mut ws, ask(1, None)).await;
        until(|| d.view(s.id).unwrap().failure.is_none()).await;
        until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.answerable)).await;
        assert_eq!(d.view(s.id).unwrap().state, SessionState::Blocked);
    })
    .await;
}

/// A Codex session whose thread doesn't start within its start's watch
/// is done, said not to have come up; its thread starting late brings it
/// up after all, that behind it.
#[tokio::test]
async fn a_codex_session_that_comes_up_late_is_up_and_its_failure_behind_it() {
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
            .create(codex_session(dir.path(), false), None)
            .await
            .unwrap();
        let mut ws = server.await.unwrap();
        next_sent(&mut ws, |v| v["method"] == "initialized").await;
        for _ in 0..100 {
            if d.view(s.id).unwrap().state != SessionState::Starting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let v = d.view(s.id).unwrap();
        assert_eq!(v.state, SessionState::Done);
        assert_eq!(v.failed.as_deref(), Some("Codex didn't come up"));
        say(
            &mut ws,
            json!({"method": "thread/started", "params": {"thread": {"id": "t1",
            "environments": [{"cwd": "/repo"}], "path": null}}}),
        )
        .await;
        until(|| d.view(s.id).unwrap().state == SessionState::Idle).await;
        assert_eq!(d.view(s.id).unwrap().failure, None);
    })
    .await;
}

/// A Desk on the stand-in, following a new Codex session's thread `t1`.
async fn following(
    pool: workforce_postgres::PostgresPool,
    dir: &Path,
) -> (Desk, crate::model::SessionView, WebSocketStream<UnixStream>) {
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
    let s = d.create(codex_session(dir, false), None).await.unwrap();
    let mut ws = server.await.unwrap();
    next_sent(&mut ws, |v| v["method"] == "initialized").await;
    say(
        &mut ws,
        json!({"method": "thread/started", "params": {"thread": {"id": "t1",
        "environments": [{"cwd": "/repo"}], "path": null}}}),
    )
    .await;
    say(&mut ws, status(&[])).await;
    let resume = next_sent(&mut ws, |v| v["method"] == "thread/resume").await;
    say(
        &mut ws,
        json!({"id": resume["id"], "result": {"thread": {"id": "t1"}}}),
    )
    .await;
    (d, s, ws)
}

/// Longer than Desk waits before it says Codex waits in its terminal.
const PAST_WAIT: Duration = Duration::from_millis(2500);

/// What its card and the person read while it waits: never that only
/// they or the terminal can answer what Desk answers.
fn no_false_wait(d: &Desk, id: uuid::Uuid) {
    let v = d.view(id).unwrap();
    assert!(
        v.pending.as_ref().is_none_or(|p| !p.in_terminal()),
        "{:?}",
        v.pending
    );
    assert!(
        v.waits_on_you
            .as_deref()
            .is_none_or(|w| !w.contains("only") && !w.contains("Only")),
        "{:?}",
        v.waits_on_you
    );
}

/// Codex's status says it waits before what it waits on comes, and after
/// Desk answered it: neither is a wait in its terminal.
#[tokio::test]
async fn a_request_desk_answers_never_reads_as_one_only_the_terminal_answers() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, s, mut ws) = following(pool, dir.path()).await;
        // Its flag first, then the request, a while later: nothing says it
        // waits in its terminal meanwhile, nor once the request lands.
        say(&mut ws, status(&["waitingOnApproval"])).await;
        until(|| d.view(s.id).unwrap().state == SessionState::Blocked).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        no_false_wait(&d, s.id);
        assert_eq!(d.view(s.id).unwrap().pending, None);
        say(&mut ws, ask(0, None)).await;
        until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.answerable)).await;
        tokio::time::sleep(PAST_WAIT).await;
        no_false_wait(&d, s.id);
        // Answered from Desk within the window, its flag still up a moment
        // after: no wait in its terminal, then or later.
        let p = d.view(s.id).unwrap().pending.unwrap();
        d.decide(s.id, p.id, Decision::Allow, Some(By::you()))
            .unwrap();
        next_sent(&mut ws, |v| v.get("result").is_some()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        no_false_wait(&d, s.id);
        say(
            &mut ws,
            json!({"method": "serverRequest/resolved",
            "params": {"threadId": "t1", "requestId": 0}}),
        )
        .await;
        say(&mut ws, status(&[])).await;
        tokio::time::sleep(PAST_WAIT).await;
        no_false_wait(&d, s.id);
        let v = d.view(s.id).unwrap();
        assert_eq!((v.state, v.pending), (SessionState::Working, None));
        assert!(
            matches!(
                d.decide(
                    s.id,
                    uuid::Uuid::new_v4(),
                    Decision::Terminal,
                    Some(By::you())
                ),
                Err(DeskError::Conflict(_) | DeskError::Invalid(_))
            ),
            "nothing to hand to the terminal"
        );
    })
    .await;
}

/// What its TUI answers, and a request Desk doesn't know, show as a wait in
/// its terminal once Codex has waited on them a while, named; the moment
/// the terminal answers, that wait goes.
#[tokio::test]
async fn what_only_its_terminal_answers_shows_named_after_a_while_and_goes_once_answered() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, s, mut ws) = following(pool, dir.path()).await;
        say(
            &mut ws,
            json!({"id": 40, "method": "item/tool/call", "params": {"threadId": "t1",
            "turnId": "u1", "callId": "c1", "namespace": null, "tool": "lookup", "arguments": {}}}),
        )
        .await;
        say(&mut ws, status(&["waitingOnApproval"])).await;
        until(|| d.view(s.id).unwrap().state == SessionState::Blocked).await;
        assert_eq!(d.view(s.id).unwrap().pending, None, "not at once");
        until(|| d.view(s.id).unwrap().pending.is_some()).await;
        let p = d.view(s.id).unwrap().pending.unwrap();
        assert!(
            p.in_terminal() && p.detail.contains("item/tool/call"),
            "{p:?}"
        );
        assert!(d
            .decide(s.id, p.id, Decision::Allow, Some(By::you()))
            .is_err());
        say(
            &mut ws,
            json!({"method": "serverRequest/resolved",
            "params": {"threadId": "t1", "requestId": 40}}),
        )
        .await;
        until(|| d.view(s.id).unwrap().pending.is_none()).await;
        assert_ne!(d.view(s.id).unwrap().state, SessionState::Blocked);
        assert!(
            ws_sent_nothing_for(&mut ws, 40).await,
            "its TUI's: Desk never answers it"
        );

        // One Desk doesn't know: said, then shown named.
        say(
            &mut ws,
            json!({"id": 41, "method": "item/someday/new", "params": {"threadId": "t1"}}),
        )
        .await;
        say(&mut ws, status(&["waitingOnUserInput"])).await;
        until(|| d.view(s.id).unwrap().pending.is_some()).await;
        let p = d.view(s.id).unwrap().pending.unwrap();
        assert!(
            p.in_terminal() && p.detail.contains("item/someday/new"),
            "{p:?}"
        );
        let mut said = false;
        for _ in 0..200 {
            said = d
                .activity(crate::crew::Filter::All, None, 50)
                .await
                .unwrap()
                .events
                .iter()
                .any(|e| e.text.contains("item/someday/new"));
            if said {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(said, "a request Desk doesn't know is said");
        // Its turn goes on: the wait is over, however Codex says so.
        say(&mut ws, status(&[])).await;
        until(|| d.view(s.id).unwrap().pending.is_none()).await;
    })
    .await;
}

/// Whether Desk sent nothing answering request `id` within a moment.
async fn ws_sent_nothing_for(ws: &mut WebSocketStream<UnixStream>, id: i64) -> bool {
    use futures::StreamExt;
    let read = tokio::time::timeout(Duration::from_millis(300), async {
        while let Some(Ok(m)) = ws.next().await {
            if let Ok(v) = serde_json::from_str::<Value>(m.to_text().unwrap_or_default()) {
                if v["id"] == id && v.get("method").is_none() {
                    return true;
                }
            }
        }
        false
    })
    .await;
    !read.unwrap_or(false)
}

/// A boss acting fully closes a Codex session by cancelling what it
/// decides: its permissions request, which has no cancel, is denied, so
/// Codex is granted nothing.
#[tokio::test]
async fn a_permissions_request_cancelled_to_close_is_granted_nothing() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, s, mut ws) = following(pool, dir.path()).await;
        // Asked before its boss takes it: what it runs in changes with the
        // charge, once nothing waits.
        say(
            &mut ws,
            json!({"id": 14, "method": "item/permissions/requestApproval",
            "params": {"threadId": "t1", "turnId": "u1", "itemId": "perm-1", "startedAtMs": 1,
            "cwd": "/repo", "reason": "Select a workspace root",
            "permissions": {"network": {"enabled": true}, "fileSystem": {"write": ["/repo"]}}}}),
        )
        .await;
        until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.answerable)).await;
        d.set_full(std::collections::HashMap::from([(
            "crew".to_string(),
            crate::charge::FullGrant {
                anywhere: true,
                ..Default::default()
            },
        )]));
        d.take_on(
            s.id,
            &crate::crew::Party::manager("crew", "Crew"),
            crate::charge::How::Handed,
            &Default::default(),
            &By::you(),
        )
        .await
        .unwrap();
        let p = d.view(s.id).unwrap().pending.unwrap();
        let boss = By::manager("crew", "Crew", "Its ticket is done.");
        assert_eq!(d.cancel_boss_pending(s.id, &boss).await.unwrap(), [p.id]);
        let sent = next_sent(&mut ws, |v| v["id"] == 14).await;
        assert_eq!(sent["result"], json!({"permissions": {}, "scope": "turn"}));
    })
    .await;
}

/// What Codex asks to run is classed from the command it runs, wrapped in
/// its shell, in the session's repository: what its boss's standing
/// verdicts are about.
#[tokio::test]
async fn a_codex_request_is_classed_by_the_command_its_shell_runs() {
    use crate::computer::boss::Class;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let origin = tempfile::tempdir().unwrap();
        let run = |at: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(at)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        run(origin.path(), &["init", "-q", "--bare", "-b", "main"]);
        let git = |args: &[&str]| run(&dir, args);
        git(&["init", "-q", "-b", "main"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ]);
        git(&["remote", "add", "origin", &origin.path().to_string_lossy()]);
        git(&["push", "-q", "origin", "main"]);
        git(&["remote", "set-head", "origin", "main"]);
        git(&["switch", "-q", "-c", "feat/x"]);
        let (d, s, mut ws) = following(pool, &dir).await;
        d.boss_bounds_with(crate::charge::Bounds {
            home: dir.clone(),
            ..Default::default()
        });
        for (id, command, class) in [
            (21, "git commit -m 'a fix'", Class::Commit),
            (22, "git push origin main", Class::DefaultBranchPush),
            (23, "git push -f origin feat/x", Class::ForcePush),
            (24, "npm test", Class::Other),
        ] {
            let mut request = ask(id, None);
            request["params"]["command"] = json!(format!("/bin/zsh -lc \"{command}\""));
            request["params"]["cwd"] = json!(dir);
            request["params"]["commandActions"] = json!([]);
            say(&mut ws, request).await;
            until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.answerable)).await;
            let p = d.view(s.id).unwrap().pending.unwrap();
            assert_eq!(d.request_class(s.id, p.id, "crew"), class, "{command}");
            d.decide(s.id, p.id, Decision::Deny, Some(By::you()))
                .unwrap();
            next_sent(&mut ws, |v| v["id"] == id).await;
            say(
                &mut ws,
                json!({"method": "serverRequest/resolved",
                "params": {"threadId": "t1", "requestId": id}}),
            )
            .await;
            until(|| d.view(s.id).unwrap().pending.is_none()).await;
        }
    })
    .await;
}

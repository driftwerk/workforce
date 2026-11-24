//! A Codex session's whole run (started, put in another mode, interrupted,
//! restarted, resumed) against a stand-in CLI that refuses what CLI 0.166
//! refuses on a resume through `--remote`, as 0.166 does: it prints why and
//! stays up.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio_tungstenite::WebSocketStream;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::codex_asks_tests::{codex_session, until};
use super::tests::options;
use super::{Desk, DeskError};
use crate::codex::tests::{next_sent, say};
use crate::crew::By;
use crate::mode::Mode;
use crate::model::{Engine, SessionState};
use crate::store::{build_pool, DeskStore};

const REFUSED: &str = "Error: Permission overrides are not supported when resuming a remote task.";

/// A stand-in `codex`: its app-server says where it was told to listen; its
/// TUI writes down how it was started, one line each, and refuses a resume
/// given a permission (or any, once `refuse` is in `dir`) as 0.166 does.
fn fake_codex(dir: &Path) -> PathBuf {
    let path = dir.join("fake-codex");
    workforce_common::executable::write(
        &path,
        format!(
            "#!/bin/sh\nd='{d}'\n\
             if [ \"$1\" = app-server ]; then echo \"${{3#unix://}}\" > \"$d/listen\"; exec sleep 60; fi\n\
             echo \"$@\" >> \"$d/tui-args\"\n\
             refuse() {{ printf '\\033[?1049h>_ OpenAI Codex (v0.166.0)\\r\\n\\033[?1049l{REFUSED}\\r\\n'; exec sleep 60; }}\n\
             if [ \"$1\" = resume ]; then\n\
             [ -e \"$d/refuse\" ] && refuse\n\
             for a in \"$@\"; do case \"$a\" in approval_policy=*|approvals_reviewer=*|sandbox_mode=*) refuse ;; esac; done\n\
             fi\n\
             exec sleep 60\n",
            d = dir.display()
        ),
    )
    .unwrap();
    path
}

/// Listens where the next app-server was told to, and takes the observer's
/// connection.
fn serve(dir: &Path) -> tokio::task::JoinHandle<WebSocketStream<UnixStream>> {
    let told = dir.join("listen");
    tokio::spawn(async move {
        let socket = loop {
            if let Ok(s) = std::fs::read_to_string(&told) {
                if !s.trim().is_empty() {
                    std::fs::remove_file(&told).unwrap();
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

/// How its TUI was started, each time: not Desk's other runs of the CLI
/// (its version, its sign-in).
fn tui_starts(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("tui-args"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("--remote") || l.starts_with("resume"))
        .map(str::to_string)
        .collect()
}

/// Desk's resume of the thread, as its observer sends it before the TUI
/// starts, answered with `answer`.
async fn resumed(ws: &mut WebSocketStream<UnixStream>, answer: Value) -> Value {
    let resume = next_sent(ws, |v| v["method"] == "thread/resume").await;
    let mut reply = json!({"id": resume["id"]});
    let key = if answer.get("message").is_some() {
        "error"
    } else {
        "result"
    };
    reply[key] = answer;
    say(ws, reply).await;
    resume["params"].clone()
}

fn plan_in(dir: &Path) -> Value {
    json!({"threadId": "t1", "excludeTurns": true, "cwd": dir,
        "approvalPolicy": "on-request", "sandbox": "read-only", "approvalsReviewer": "user"})
}

fn status(kind: &str) -> Value {
    json!({"method": "thread/status/changed",
        "params": {"threadId": "t1", "status": {"type": kind, "activeFlags": []}}})
}

async fn stopped(d: &Desk, id: Uuid) {
    d.stop(id, Some(By::you())).unwrap();
    until(|| !d.view(id).unwrap().live).await;
}

/// Ended as it started: what it said is kept, the session not running.
async fn ended_as_started(d: &Desk, id: Uuid) -> crate::model::Exited {
    until(|| {
        let v = d.view(id).unwrap();
        !v.live && v.ended_as_started().is_some()
    })
    .await;
    d.view(id).unwrap().exited.unwrap()
}

#[tokio::test]
async fn a_codex_session_keeps_its_mode_through_every_start_and_a_refusal_is_said_not_looped() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let d = Desk::start(
            DeskStore::from_pool(pool),
            options(&dir).with_program(Engine::Codex, fake_codex(&dir)),
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let boss = || By::manager("crew", "Crew", "it should go on");

        // Started: its TUI starts the thread, which Desk then follows.
        let server = serve(&dir);
        let s = d.create(codex_session(&dir, false), None).await.unwrap();
        let mut ws = server.await.unwrap();
        say(&mut ws, json!({"method": "thread/started", "params": {"thread": {"id": "t1",
            "environments": [{"cwd": dir}], "path": null}}})).await;
        say(&mut ws, status("idle")).await;
        resumed(&mut ws, json!({"thread": {"id": "t1"}})).await;
        until(|| d.record(s.id).is_ok_and(|r| r.cli_session_id.as_deref() == Some("t1"))).await;
        until(|| tui_starts(&dir).len() == 1).await;
        assert!(tui_starts(&dir)[0].starts_with("--remote"), "{:?}", tui_starts(&dir));

        // Put in plan mode: started again in place, its thread resumed in
        // plan mode by Desk before its TUI joins it, with nothing it refuses.
        let server = serve(&dir);
        let set = tokio::spawn({
            let d = d.clone();
            async move { d.set_mode(s.id, Mode::Plan, By::you()).await }
        });
        let mut ws = server.await.unwrap();
        assert_eq!(resumed(&mut ws, json!({"thread": {"id": "t1"}})).await, plan_in(&dir));
        let set = set.await.unwrap().unwrap();
        assert!(set.running && set.not_started.is_none(), "{set:?}");
        until(|| tui_starts(&dir).len() == 2).await;
        let started = tui_starts(&dir)[1].clone();
        assert!(started.starts_with("resume t1 --remote"), "{started}");
        assert!(!started.contains("approval") && !started.contains("sandbox"), "{started}");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(d.view(s.id).unwrap().live, "its TUI took the resume");

        // Interrupted mid-turn: its turn stops, and it isn't started again.
        say(&mut ws, status("active")).await;
        until(|| d.view(s.id).unwrap().state == SessionState::Working).await;
        d.interrupt(s.id, Some(By::you())).unwrap();
        say(&mut ws, status("idle")).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(d.view(s.id).unwrap().live);
        assert_eq!(tui_starts(&dir).len(), 2, "no start of its own");

        // Restarted, then stopped and resumed as Desk's own restore does:
        // still in plan mode each time.
        let server = serve(&dir);
        let restart = tokio::spawn({
            let d = d.clone();
            async move { d.restart(s.id, None, By::you()).await }
        });
        let mut ws = server.await.unwrap();
        assert_eq!(resumed(&mut ws, json!({"thread": {"id": "t1"}})).await, plan_in(&dir));
        restart.await.unwrap().unwrap();
        until(|| tui_starts(&dir).len() == 3).await;
        stopped(&d, s.id).await;
        let server = serve(&dir);
        let restore = tokio::spawn({
            let d = d.clone();
            async move { d.resume(s.id, None, None).await }
        });
        let mut ws = server.await.unwrap();
        assert_eq!(resumed(&mut ws, json!({"thread": {"id": "t1"}})).await, plan_in(&dir));
        restore.await.unwrap().unwrap();
        until(|| tui_starts(&dir).len() == 4).await;
        assert!(tui_starts(&dir).iter().skip(1).all(|a| !a.contains("approval")));

        // Its TUI refuses, says why and stays up: ended as it started, with
        // what it said, never left starting.
        stopped(&d, s.id).await;
        std::fs::write(dir.join("refuse"), "").unwrap();
        for _ in 0..2 {
            let server = serve(&dir);
            let again = tokio::spawn({
                let d = d.clone();
                async move { d.resume(s.id, None, Some(boss())).await }
            });
            let mut ws = server.await.unwrap();
            resumed(&mut ws, json!({"thread": {"id": "t1"}})).await;
            again.await.unwrap().unwrap();
            let e = ended_as_started(&d, s.id).await;
            assert!(e.said(400).contains(REFUSED), "{e:?}");
            assert!(e.told().contains(REFUSED), "{}", e.told());
        }
        assert_eq!(d.view(s.id).unwrap().exited.unwrap().repeats, 1);

        // The same end twice in a row: its boss isn't let start it again,
        // and is told why in words; nothing starts.
        let starts = tui_starts(&dir).len();
        for again in [
            d.resume(s.id, None, Some(boss())).await,
            d.restart(s.id, None, boss()).await,
        ] {
            assert!(
                matches!(&again, Err(DeskError::Conflict(why))
                    if why.contains("2 times in a row") && why.contains(REFUSED)),
                "{again:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!dir.join("listen").exists(), "no app-server started");
        assert_eq!(tui_starts(&dir).len(), starts);

        // The person still may; and what Codex's app-server refuses is said
        // the same way, its TUI never started.
        let server = serve(&dir);
        let theirs = tokio::spawn({
            let d = d.clone();
            async move { d.resume(s.id, None, Some(By::you())).await }
        });
        let mut ws = server.await.unwrap();
        resumed(&mut ws, json!({"code": -32600, "message": "no rollout found for thread id t1"}))
            .await;
        let refused = theirs.await.unwrap();
        assert!(
            matches!(&refused, Err(DeskError::Conflict(why))
                if why == "Codex didn't resume its conversation: no rollout found for thread id t1"),
            "{refused:?}"
        );
        let e = ended_as_started(&d, s.id).await;
        assert_eq!(e.repeats, 0, "a different end");
        assert_eq!(tui_starts(&dir).len(), starts, "its TUI never started");

        // Fixed, the person's resume runs it in its mode again.
        std::fs::remove_file(dir.join("refuse")).unwrap();
        let server = serve(&dir);
        let fixed = tokio::spawn({
            let d = d.clone();
            async move { d.resume(s.id, None, Some(By::you())).await }
        });
        let mut ws = server.await.unwrap();
        assert_eq!(resumed(&mut ws, json!({"thread": {"id": "t1"}})).await, plan_in(&dir));
        fixed.await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        let v = d.view(s.id).unwrap();
        assert!(v.live && v.exited.is_none(), "{v:?}");
    })
    .await;
}

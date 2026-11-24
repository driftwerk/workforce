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
use workforce_postgres::{test_support::with_test_pool, PostgresPool};

use super::codex_asks_tests::{codex_session, until};
use super::tests::options;
use super::{Desk, DeskError};
use crate::codex::tests::{next_sent, say};
use crate::crew::By;
use crate::mode::Mode;
use crate::model::{Engine, SessionState};
use crate::store::{build_pool, DeskStore};

const REFUSED: &str = "Error: Permission overrides are not supported when resuming a remote task.";

/// A stand-in `codex`: Desk's other runs of it (its version, its sign-in)
/// end at once; its app-server says where it was told to listen; its
/// TUI writes down how it was started, one line each, and refuses a resume
/// given a permission (or any, once `refuse` is in `dir`) as 0.166 does;
/// else it draws its screen, the alternate one, unless `hang` is in `dir`.
fn fake_codex(dir: &Path) -> PathBuf {
    let path = dir.join("fake-codex");
    workforce_common::executable::write(
        &path,
        format!(
            "#!/bin/sh\nd='{d}'\n\
             case \"$1\" in app-server|--remote|resume) ;; *) exit 0 ;; esac\n\
             if [ \"$1\" = app-server ]; then echo \"${{3#unix://}}\" > \"$d/listen\"; exec sleep 60; fi\n\
             echo \"$@\" >> \"$d/tui-args\"\n\
             refuse() {{ printf '\\033[?1049h>_ OpenAI Codex (v0.166.0)\\r\\n\\033[?1049l{REFUSED}\\r\\n'; exec sleep 60; }}\n\
             if [ \"$1\" = resume ]; then\n\
             [ -e \"$d/refuse\" ] && refuse\n\
             for a in \"$@\"; do case \"$a\" in approval_policy=*|approvals_reviewer=*|sandbox_mode=*) refuse ;; esac; done\n\
             fi\n\
             [ -e \"$d/hang\" ] || printf '\\033[?1049h>_ OpenAI Codex (v0.166.0)\\r\\n'\n\
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
    in_mode(dir, "on-request", "read-only")
}

fn in_mode(dir: &Path, approval: &str, sandbox: &str) -> Value {
    json!({"threadId": "t1", "excludeTurns": true, "cwd": dir,
        "approvalPolicy": approval, "sandbox": sandbox, "approvalsReviewer": "user"})
}

/// In what it last ran in: the person's own, from Codex's settings or its
/// terminal.
fn its_own(dir: &Path) -> Value {
    json!({"threadId": "t1", "excludeTurns": true, "cwd": dir})
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

/// A Desk on the stand-in, and a Codex session on it whose TUI has
/// started its thread (`t1`), which Desk follows.
async fn started(pool: PostgresPool, dir: &Path) -> (Desk, crate::model::SessionView) {
    DeskStore::migrate(&pool).await.unwrap();
    let d = desk_on(pool, dir, CancellationToken::new()).await;
    let server = serve(dir);
    let s = d.create(codex_session(dir, false), None).await.unwrap();
    let mut ws = server.await.unwrap();
    say(
        &mut ws,
        json!({"method": "thread/started", "params": {"thread": {"id": "t1",
        "environments": [{"cwd": dir}], "path": null}}}),
    )
    .await;
    say(&mut ws, status("idle")).await;
    resumed(&mut ws, json!({"thread": {"id": "t1"}})).await;
    until(|| {
        d.record(s.id)
            .is_ok_and(|r| r.cli_session_id.as_deref() == Some("t1"))
    })
    .await;
    until(|| tui_starts(dir).len() == 1).await;
    assert!(
        tui_starts(dir)[0].starts_with("--remote"),
        "{:?}",
        tui_starts(dir)
    );
    (d, s)
}

async fn desk_on(pool: PostgresPool, dir: &Path, cancel: CancellationToken) -> Desk {
    Desk::start(
        DeskStore::from_pool(pool),
        options(dir).with_program(Engine::Codex, fake_codex(dir)),
        Default::default(),
        cancel,
    )
    .await
    .unwrap()
}

/// Crew, acting fully and unbound, runs `id`.
async fn act_fully(d: &Desk, id: Uuid) {
    d.set_full(std::collections::HashMap::from([(
        "crew".to_string(),
        crate::charge::FullGrant {
            anywhere: true,
            unbound: true,
            ..Default::default()
        },
    )]));
    d.take_on(
        id,
        &crate::crew::Party::manager("crew", "Crew"),
        crate::charge::How::Handed,
        &Default::default(),
        &By::you(),
    )
    .await
    .unwrap();
}

/// Starts it again with `again`, its thread resumed by Desk as `answers`
/// say, one by one: the params of each, and its app-server's end.
async fn started_again<F>(
    d: &Desk,
    dir: &Path,
    answers: &[Value],
    again: impl FnOnce(Desk) -> F,
) -> (Vec<Value>, WebSocketStream<UnixStream>)
where
    F: std::future::Future<Output = Result<(), DeskError>> + Send + 'static,
{
    let server = serve(dir);
    let again = tokio::spawn(again(d.clone()));
    let mut ws = server.await.unwrap();
    let mut asked = Vec::new();
    for answer in answers {
        asked.push(resumed(&mut ws, answer.clone()).await);
    }
    again.await.unwrap().unwrap();
    (asked, ws)
}

/// A turn runs in its thread, from its start to its end.
async fn turn(d: &Desk, id: Uuid, ws: &mut WebSocketStream<UnixStream>) {
    say(ws, status("active")).await;
    until(|| d.view(id).unwrap().state == SessionState::Working).await;
    say(ws, status("idle")).await;
    until(|| d.view(id).unwrap().state != SessionState::Working).await;
}

/// A session in its own mode is resumed in it, whoever starts it, never
/// put in Codex's default; one a boss left in a mode that doesn't ask is
/// put back in Codex's default once that mode goes, and a thread its last
/// app-server still holds as it ends is waited for. That mode's going is
/// kept with Desk until the session has started out of it: through a start
/// that fails, and a hub's restart.
#[tokio::test]
async fn a_codex_session_runs_in_its_own_mode_unless_one_that_doesnt_ask_is_kept() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let (d, s) = started(pool.clone(), &dir).await;
        let ok = json!({"thread": {"id": "t1"}});
        let tui_has_no_permissions = || {
            tui_starts(&dir)
                .iter()
                .skip(1)
                .all(|a| !a.contains("approval") && !a.contains("sandbox"))
        };

        // Restarted as its last app-server still holds the thread: asked
        // again, in its own mode, and it runs.
        let held = json!({"code": -32603, "message": "thread t1 already has an active writer"});
        let (asked, _) = started_again(&d, &dir, &[held, ok.clone()], move |d| async move {
            d.restart(s.id, None, By::you()).await.map(drop)
        })
        .await;
        assert_eq!(asked, [its_own(&dir), its_own(&dir)]);
        until(|| tui_starts(&dir).len() == 2).await;
        assert!(tui_has_no_permissions());
        let v = d.view(s.id).unwrap();
        assert!(v.live && v.exited.is_none(), "{v:?}");

        // A boss's restart: still its own.
        let boss = || By::manager("crew", "Crew", "it should go on");
        let (asked, _) = started_again(&d, &dir, std::slice::from_ref(&ok), move |d| async move {
            d.restart(s.id, None, boss()).await.map(drop)
        })
        .await;
        assert_eq!(asked, [its_own(&dir)]);
        until(|| tui_starts(&dir).len() == 3).await;

        // Acting fully, an unbound boss that runs it bypasses its
        // permissions, and its restart keeps it so.
        act_fully(&d, s.id).await;
        let bypassed = in_mode(&dir, "never", "danger-full-access");
        let (asked, _) = started_again(&d, &dir, std::slice::from_ref(&ok), move |d| async move {
            d.set_mode(s.id, Mode::BypassPermissions, boss())
                .await
                .map(drop)
        })
        .await;
        assert_eq!(asked, std::slice::from_ref(&bypassed));
        until(|| tui_starts(&dir).len() == 4).await;
        let (asked, _) = started_again(&d, &dir, std::slice::from_ref(&ok), move |d| async move {
            d.restart(s.id, None, boss()).await.map(drop)
        })
        .await;
        assert_eq!(asked, [bypassed]);
        until(|| tui_starts(&dir).len() == 5).await;

        // The person resumes it: that mode goes, and the thread is put back
        // in Codex's default, not left bypassing, even when that start fails
        // or its TUI ends before a turn has run in it; once one has, it is
        // its own again.
        let reset = in_mode(&dir, "on-request", "workspace-write");
        stopped(&d, s.id).await;
        let server = serve(&dir);
        let failed = tokio::spawn({
            let d = d.clone();
            async move { d.resume(s.id, None, Some(By::you())).await }
        });
        let mut ws = server.await.unwrap();
        let lost = json!({"code": -32603, "message": "no rollout found for thread id t1"});
        assert_eq!(resumed(&mut ws, lost).await, reset);
        assert!(failed.await.unwrap().is_err());
        let (asked, _) = started_again(&d, &dir, std::slice::from_ref(&ok), move |d| async move {
            d.resume(s.id, None, Some(By::you())).await.map(drop)
        })
        .await;
        assert_eq!(asked, std::slice::from_ref(&reset));
        until(|| tui_starts(&dir).len() == 6).await;
        stopped(&d, s.id).await;
        let (asked, mut ws) =
            started_again(&d, &dir, std::slice::from_ref(&ok), move |d| async move {
                d.resume(s.id, None, Some(By::you())).await.map(drop)
            })
            .await;
        assert_eq!(asked, std::slice::from_ref(&reset), "no turn ran in it");
        until(|| tui_starts(&dir).len() == 7).await;
        turn(&d, s.id, &mut ws).await;
        stopped(&d, s.id).await;
        let (asked, _) = started_again(&d, &dir, std::slice::from_ref(&ok), move |d| async move {
            d.resume(s.id, None, Some(By::you())).await.map(drop)
        })
        .await;
        assert_eq!(asked, [its_own(&dir)]);
        until(|| tui_starts(&dir).len() == 8).await;

        // Bypassing again, stopped, and the hub restarted: its next hub
        // finds the mode gone, and puts the thread back in Codex's default.
        act_fully(&d, s.id).await;
        let (asked, _) = started_again(&d, &dir, std::slice::from_ref(&ok), move |d| async move {
            d.set_mode(s.id, Mode::BypassPermissions, boss())
                .await
                .map(drop)
        })
        .await;
        assert_eq!(asked, [in_mode(&dir, "never", "danger-full-access")]);
        until(|| tui_starts(&dir).len() == 9).await;
        stopped(&d, s.id).await;
        d.0.cancel.cancel();
        drop(d);
        let d = desk_on(pool, &dir, CancellationToken::new()).await;
        let (asked, mut ws) =
            started_again(&d, &dir, std::slice::from_ref(&ok), move |d| async move {
                d.resume(s.id, None, Some(By::you())).await.map(drop)
            })
            .await;
        assert_eq!(asked, [reset]);
        until(|| tui_starts(&dir).len() == 10).await;
        until(|| d.view(s.id).unwrap().state == SessionState::Idle).await;
        turn(&d, s.id, &mut ws).await;
        stopped(&d, s.id).await;
        let (asked, _) = started_again(&d, &dir, &[ok], move |d| async move {
            d.resume(s.id, None, Some(By::you())).await.map(drop)
        })
        .await;
        assert_eq!(asked, [its_own(&dir)]);
        until(|| tui_starts(&dir).len() == 11).await;
        assert!(tui_has_no_permissions());
    })
    .await;
}

/// What the session's warnings said, as the test's runtime runs it.
#[derive(Clone, Default)]
struct Said(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Said {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Said {
    fn has(&self, what: &str) -> bool {
        String::from_utf8_lossy(&self.0.lock().unwrap()).contains(what)
    }
}

/// A resumed session is up once its TUI is on its screen, never on
/// Desk's own resume of its thread: one that never draws stays starting,
/// and is said so; one that never starts is never up.
#[tokio::test]
async fn a_resumed_codex_session_is_up_only_once_its_tui_is_on_its_screen() {
    let said = Said::default();
    let _said = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer({
                let said = said.clone();
                move || said.clone()
            })
            .finish(),
    );
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let (d, s) = started(pool, &dir).await;
        let ok = json!({"thread": {"id": "t1"}});
        let resume =
            move |d: Desk| async move { d.resume(s.id, None, Some(By::you())).await.map(drop) };
        let state = || d.view(s.id).unwrap().state;

        // On its screen: up. It is told to take it, whatever the person's
        // settings say: one running inline never would.
        stopped(&d, s.id).await;
        started_again(&d, &dir, std::slice::from_ref(&ok), resume).await;
        until(|| state() == SessionState::Idle).await;
        let last = tui_starts(&dir).pop().unwrap();
        assert!(last.contains("tui.alternate_screen=\"always\""), "{last}");

        // Its thread loaded, but its TUI never takes its screen: still starting, and
        // said so once its start's watch is over.
        stopped(&d, s.id).await;
        std::fs::write(dir.join("hang"), "").unwrap();
        started_again(&d, &dir, std::slice::from_ref(&ok), resume).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(state(), SessionState::Starting);
        assert!(d.view(s.id).unwrap().live);
        until(|| said.has("a Codex session is still starting")).await;
        assert_eq!(state(), SessionState::Starting);
        stopped(&d, s.id).await;
        std::fs::remove_file(dir.join("hang")).unwrap();

        // Its thread loaded, and then its TUI can't start: never up.
        let program = dir.join("fake-codex");
        let mode = |m| {
            std::fs::set_permissions(&program, std::os::unix::fs::PermissionsExt::from_mode(m))
                .unwrap()
        };
        let server = serve(&dir);
        let failed = tokio::spawn(resume(d.clone()));
        let mut ws = server.await.unwrap();
        mode(0o644);
        resumed(&mut ws, ok.clone()).await;
        let _ = failed.await.unwrap();
        mode(0o755);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let v = d.view(s.id).unwrap();
        assert!(!v.live && v.state != SessionState::Idle, "{v:?}");

        // Started again: up.
        started_again(&d, &dir, std::slice::from_ref(&ok), resume).await;
        until(|| state() == SessionState::Idle).await;
    })
    .await;
}

#[tokio::test]
async fn a_codex_session_keeps_its_mode_through_every_start_and_a_refusal_is_said_not_looped() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let (d, s) = started(pool, &dir).await;
        let boss = || By::manager("crew", "Crew", "it should go on");

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
        until(|| d.view(s.id).unwrap().state == SessionState::Idle).await;

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

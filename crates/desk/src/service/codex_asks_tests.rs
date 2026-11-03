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
use crate::model::{Decision, Engine, NewSession, SessionState};
use crate::store::{build_pool, DeskStore};

/// A stand-in `codex`: as `app-server --listen unix://…` it says where it
/// was told to listen, for the test to listen there; as the TUI it waits.
fn fake_codex(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-codex");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n[ \"$1\" = app-server ] && echo \"${{3#unix://}}\" > '{}/listen'\nexec sleep 60\n",
            dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Listens where the stand-in was told to, and takes the observer's
/// connection.
fn serve(dir: &Path) -> tokio::task::JoinHandle<WebSocketStream<UnixStream>> {
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

async fn until(mut ok: impl FnMut() -> bool) {
    for _ in 0..300 {
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
            },
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
                    name: "cleanup".into(),
                    cwd: dir.path().to_string_lossy().into_owned(),
                    goal: String::new(),
                    worktree: None,
                    prompt: None,
                    approvals: None,
                    env: Default::default(),
                },
                None,
            )
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

        // It says it waits, then what for: the same request, answerable.
        say(&mut ws, status(&["waitingOnApproval"])).await;
        until(|| d.view(s.id).unwrap().pending.is_some()).await;
        let waited = d.view(s.id).unwrap().pending.unwrap();
        say(&mut ws, ask(0, Some(json!(["accept", {"acceptWithExecpolicyAmendment":
            {"execpolicy_amendment": ["rm", "a.txt"]}}, "cancel"])))).await;
        until(|| d.view(s.id).unwrap().pending.is_some_and(|p| p.answerable)).await;
        let v = d.view(s.id).unwrap();
        let p = v.pending.unwrap();
        assert_eq!(v.state, SessionState::Blocked);
        assert_eq!(p.id, waited.id, "the request it said it waited on");
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
        d.decide(s.id, p.id, Decision::Cancel, you()).unwrap();
        let answer = next_sent(&mut ws, |v| v.get("result").is_some()).await;
        assert_eq!(answer, json!({"id": 0, "result": {"decision": "cancel"}}));
        assert!(d.view(s.id).unwrap().pending.is_none());
        assert!(
            matches!(d.decide(s.id, p.id, Decision::Allow, you()), Err(DeskError::Conflict(_))),
            "answered once"
        );
        say(&mut ws, json!({"method": "serverRequest/resolved",
            "params": {"threadId": "t1", "requestId": 0}})).await;

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

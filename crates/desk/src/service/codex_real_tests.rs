//! Desk against the real Codex CLI 0.166 (`WORKFORCE_CODEX_BIN`) in its
//! pty, with a stand-in Responses API for its model in a scratch
//! `CODEX_HOME`. Skipped where no CLI is given, unless
//! `WORKFORCE_CODEX_REQUIRED=1` says it must run (CI).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::response::IntoResponse;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use workforce_postgres::test_support::with_test_pool;

use super::codex_asks_tests::codex_session;
use super::tests::options;
use super::Desk;
use crate::crew::By;
use crate::model::{Decision, Engine, SessionState};
use crate::store::{build_pool, DeskStore};

/// The CLI under test: none where it isn't given and needn't be.
fn codex_bin() -> Option<PathBuf> {
    let bin = std::env::var_os("WORKFORCE_CODEX_BIN").map(PathBuf::from);
    assert!(
        bin.is_some() || std::env::var("WORKFORCE_CODEX_REQUIRED").as_deref() != Ok("1"),
        "WORKFORCE_CODEX_REQUIRED=1 but WORKFORCE_CODEX_BIN isn't set"
    );
    bin
}

/// The stand-in's reply: SSE events, as the Responses API streams them.
fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn reply(id: &str, item: Value) -> String {
    sse(&[
        json!({"type": "response.created", "response": {"id": id}}),
        json!({"type": "response.output_item.done", "item": item}),
        json!({"type": "response.completed", "response": {"id": id, "usage": {
            "input_tokens": 0, "input_tokens_details": null, "output_tokens": 0,
            "output_tokens_details": null, "total_tokens": 0}}}),
    ])
}

/// What the stand-in model was sent, each request's body.
type Heard = Arc<parking_lot::Mutex<Vec<Value>>>;

/// The stand-in model: asked to make the file, it runs `touch` past the
/// sandbox; asked for more room, it asks for a folder beside the
/// workspace; given what came of it, it says it is done; else it says ok.
async fn model(dir: &Path) -> (String, Heard) {
    let heard = Heard::default();
    let cmd = format!("touch {}", dir.join("made.txt").display());
    let app = axum::Router::new().route(
        "/v1/responses",
        axum::routing::post({
            let heard = heard.clone();
            move |body: Bytes| async move {
                let body: Value = serde_json::from_slice(&body).unwrap_or_default();
                let text = body.to_string();
                heard.lock().push(body);
                let out = if text.contains("function_call_output") {
                    reply(
                        "r2",
                        json!({"type": "message", "role": "assistant", "id": "m2",
                        "content": [{"type": "output_text", "text": "done"}]}),
                    )
                } else if text.contains("ask for more room") {
                    let args = json!({"reason": "Write beside the workspace",
                        "permissions": {"file_system": {"write": ["../shared"]}}});
                    reply(
                        "r3",
                        json!({"type": "function_call", "call_id": "c3",
                        "name": "request_permissions", "arguments": args.to_string()}),
                    )
                } else if text.contains("make the file") {
                    let args = json!({"cmd": cmd, "sandbox_permissions": "require_escalated",
                        "justification": "make the file"});
                    reply(
                        "r1",
                        json!({"type": "function_call", "call_id": "c1",
                        "name": "exec_command", "arguments": args.to_string()}),
                    )
                } else {
                    reply(
                        "r0",
                        json!({"type": "message", "role": "assistant", "id": "m0",
                        "content": [{"type": "output_text", "text": "ok"}]}),
                    )
                };
                ([("content-type", "text/event-stream")], out).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (url, heard)
}

/// A scratch `CODEX_HOME` on the stand-in, `dir` trusted.
fn codex_home(home: &Path, url: &str, dir: &Path) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "model = \"mock-model\"\napproval_policy = \"on-request\"\n\
             sandbox_mode = \"workspace-write\"\nmodel_provider = \"mock\"\n\
             check_for_update_on_startup = false\n\n\
             [features]\nunified_exec = true\nshell_snapshot = false\n\
             request_permissions_tool = true\n\n\
             [model_providers.mock]\nname = \"mock\"\nbase_url = \"{url}\"\n\
             wire_api = \"responses\"\nrequest_max_retries = 0\nstream_max_retries = 0\n\
             supports_websockets = false\n\n\
             [projects.\"{}\"]\ntrust_level = \"trusted\"\n",
            dir.display()
        ),
    )
    .unwrap();
}

async fn until(what: &str, within: Duration, mut ok: impl FnMut() -> bool) {
    let end = tokio::time::Instant::now() + within;
    while !ok() {
        assert!(tokio::time::Instant::now() < end, "not in time: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The real Codex asks Desk to go past its sandbox: it shows Blocked with
/// its command, the person's answer runs it within seconds, and Codex runs
/// its thread in what Desk asked. A boss taking it on between turns starts
/// it again in place, its conversation kept, under the boss.
#[tokio::test(flavor = "multi_thread")]
async fn the_real_codex_asks_desk_and_runs_what_it_allowed() {
    let Some(bin) = codex_bin() else {
        return;
    };
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let (url, heard) = model(&work).await;
        let home = dir.join("codex-home");
        codex_home(&home, &url, &work);
        let mut opts = options(&dir).with_program(Engine::Codex, bin);
        opts.env = std::collections::BTreeMap::from([
            ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
            ("HOME".into(), dir.to_string_lossy().into_owned()),
            ("CODEX_HOME".into(), home.to_string_lossy().into_owned()),
            ("OPENAI_API_KEY".into(), "sk-test-not-real".into()),
            ("TERM".into(), "xterm-256color".into()),
        ]);
        DeskStore::migrate(&pool).await.unwrap();
        let d = Desk::start(
            DeskStore::from_pool(pool),
            opts,
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        // Asked: Blocked on its command, answerable from Desk.
        let asks = async |d: &Desk, prompt: &str, tool: &str| {
            let mut new = codex_session(&work, false);
            new.prompt = Some(prompt.into());
            let s = d.create(new, None).await.unwrap();
            until("its request", Duration::from_secs(60), || {
                d.view(s.id).unwrap().pending.is_some_and(|p| p.answerable)
            })
            .await;
            let v = d.view(s.id).unwrap();
            let p = v.pending.clone().unwrap();
            assert_eq!(v.state, SessionState::Blocked);
            assert_eq!(p.tool, tool, "{p:?}");
            (s, p)
        };
        let ended = |d: &Desk, id| {
            let v = d.view(id).unwrap();
            v.pending.is_none() && !matches!(v.state, SessionState::Working | SessionState::Blocked)
        };

        // Its permissions request denied: granted nothing, Codex goes on to
        // the end of its turn.
        let (denied, p) = asks(&d, "ask for more room", "Permissions").await;
        assert_eq!(p.choices, [Decision::Allow, Decision::Deny]);
        d.decide(denied.id, p.id, Decision::Deny, Some(By::you()))
            .unwrap();
        until("its turn's end", Duration::from_secs(30), || {
            ended(&d, denied.id)
        })
        .await;
        assert!(d.view(denied.id).unwrap().failure.is_none());
        assert!(heard
            .lock()
            .iter()
            .any(|b| b.to_string().contains("function_call_output")));
        heard.lock().clear();

        let (s, p) = asks(&d, "make the file", "Bash").await;
        assert!(p.detail.contains("touch"), "{p:?}");
        // Allowed: it runs within seconds, and the turn ends.
        d.decide(s.id, p.id, Decision::Allow, Some(By::you()))
            .unwrap();
        until("what it allowed to run", Duration::from_secs(5), || {
            work.join("made.txt").exists()
        })
        .await;
        until("its turn's end", Duration::from_secs(30), || {
            ended(&d, s.id)
        })
        .await;
        assert!(heard
            .lock()
            .iter()
            .any(|b| b.to_string().contains("function_call_output")));
        let thread = d.record(s.id).unwrap().cli_session_id.expect("its thread");

        // Taken on by a boss between turns: started again in place, in
        // the same thread, under the boss's permissions.
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
        let said = async |what: &str| {
            d.activity(crate::crew::Filter::All, None, 200)
                .await
                .unwrap()
                .events
                .into_iter()
                .filter(|e| e.text.contains(what))
                .count()
        };
        let end = tokio::time::Instant::now() + Duration::from_secs(60);
        while said("again in place in the permissions it now runs in").await == 0 {
            assert!(
                tokio::time::Instant::now() < end,
                "not started again in place"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        until("it runs again", Duration::from_secs(60), || {
            d.view(s.id).unwrap().live
        })
        .await;
        assert_eq!(d.record(s.id).unwrap().cli_session_id, Some(thread.clone()));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(said("other permissions than Desk asked").await, 0);

        // Started again by its boss in what it runs in: the same thread, its
        // conversation kept, and the same permissions.
        until("between turns", Duration::from_secs(30), || ended(&d, s.id)).await;
        d.restart(
            s.id,
            Some("say ok".into()),
            By::manager("crew", "Crew", "it should go on"),
        )
        .await
        .unwrap();
        until("its next turn", Duration::from_secs(60), || {
            heard.lock().iter().any(|b| {
                let b = b.to_string();
                b.contains("say ok") && b.contains("make the file")
            })
        })
        .await;
        until("its turn's end", Duration::from_secs(30), || {
            ended(&d, s.id)
        })
        .await;
        assert_eq!(d.record(s.id).unwrap().cli_session_id, Some(thread));
        assert_eq!(said("other permissions than Desk asked").await, 0);
    })
    .await;
}

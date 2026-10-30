//! Codex: the real `codex` TUI attached to an app-server the hub runs per
//! session (`--remote unix://…`). The hub joins the same app-server as a
//! second client and reads the session's state from its notifications, so
//! nothing is scraped off the screen.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eyre::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::claude::tool_summary;
use crate::pty::Spawn;
use crate::usage::{MacAccount, MacPlan, PlanSink};

pub enum Launch<'a> {
    New,
    Resume { thread_id: &'a str },
}

pub struct CodexSpawn<'a> {
    pub program: &'a Path,
    pub cwd: &'a Path,
    pub socket: &'a Path,
    pub launch: Launch<'a>,
    pub prompt: Option<&'a str>,
    pub env: &'a BTreeMap<String, String>,
}

pub fn spawn(s: &CodexSpawn<'_>) -> Spawn {
    let remote = format!("unix://{}", s.socket.display());
    let mut args = match s.launch {
        Launch::New => vec![],
        Launch::Resume { thread_id } => vec!["resume".to_string(), thread_id.to_string()],
    };
    args.extend([
        "--remote".to_string(),
        remote,
        "-C".to_string(),
        s.cwd.display().to_string(),
    ]);
    args.extend(s.prompt.map(str::to_string));
    Spawn {
        program: s.program.to_path_buf(),
        args,
        cwd: s.cwd.to_path_buf(),
        env: s.env.clone(),
    }
}

/// How to run a session's app-server, listening on `socket`.
pub fn app_server(program: &Path, socket: &Path, env: &BTreeMap<String, String>) -> Spawn {
    Spawn {
        program: program.to_path_buf(),
        args: vec![
            "app-server".into(),
            "--listen".into(),
            format!("unix://{}", socket.display()),
        ],
        cwd: socket
            .parent()
            .map_or_else(|| PathBuf::from("/"), Path::to_path_buf),
        env: env.clone(),
    }
}

/// Makes `socket`'s folder and clears a stale socket, before a server starts.
pub async fn prepare_socket(socket: &Path) -> Result<()> {
    if let Some(dir) = socket.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let _ = tokio::fs::remove_file(socket).await;
    Ok(())
}

/// Waits up to ten seconds for a server to open `socket`.
pub async fn wait_for_socket(socket: &Path) -> Result<()> {
    for _ in 0..100 {
        if tokio::fs::try_exists(socket).await.unwrap_or(false) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eyre::bail!("codex app-server did not open {}", socket.display())
}

/// The per-session app-server as the hub's own child. Killed when dropped.
pub struct AppServer {
    _child: Child,
    pub socket: PathBuf,
}

impl AppServer {
    pub async fn start(
        program: &Path,
        socket: PathBuf,
        env: &BTreeMap<String, String>,
    ) -> Result<Self> {
        prepare_socket(&socket).await?;
        let spec = app_server(program, &socket, env);
        let child = Command::new(&spec.program)
            .args(&spec.args)
            .env_clear()
            .envs(env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("starting codex app-server")?;
        wait_for_socket(&socket).await?;
        Ok(Self {
            _child: child,
            socket,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CodexEvent {
    /// A conversation (not one of Codex's own side threads) started; its
    /// rollout file, when the server says where.
    Thread {
        rollout: Option<String>,
    },
    Working {
        waiting: bool,
    },
    Idle,
    Prompt(String),
    Tool(String),
    Reply(String),
}

/// What happened, and on which thread.
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    pub thread: String,
    pub event: CodexEvent,
}

/// Maps one app-server notification to what the session card shows.
/// Verified on CLI 0.164: `thread/started` and `thread/status/changed`
/// reach every client; item notifications only reach a thread's subscribers.
pub fn interpret(method: &str, params: &Value) -> Option<Seen> {
    let s = |p: &str| {
        params
            .pointer(p)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let seen = |thread: Option<String>, event| thread.map(|thread| Seen { thread, event });
    match method {
        "thread/started" => {
            let ephemeral =
                params.pointer("/thread/ephemeral").and_then(Value::as_bool) == Some(true);
            let side = params
                .pointer("/thread/environments")
                .and_then(Value::as_array)
                .is_some_and(|e| e.is_empty());
            if ephemeral || side {
                return None;
            }
            seen(
                s("/thread/id"),
                CodexEvent::Thread {
                    rollout: s("/thread/path"),
                },
            )
        }
        "thread/status/changed" => {
            let event = match params.pointer("/status/type").and_then(Value::as_str)? {
                "active" => CodexEvent::Working {
                    waiting: params
                        .pointer("/status/activeFlags")
                        .and_then(Value::as_array)
                        .is_some_and(|f| {
                            f.iter()
                                .any(|x| x == "waitingOnApproval" || x == "waitingOnUserInput")
                        }),
                },
                "idle" => CodexEvent::Idle,
                _ => return None,
            };
            seen(s("/threadId"), event)
        }
        "item/started" => {
            let event = match params.pointer("/item/type").and_then(Value::as_str)? {
                "userMessage" => CodexEvent::Prompt(
                    params
                        .pointer("/item/content")
                        .and_then(Value::as_array)
                        .and_then(|c| c.iter().find_map(|i| i.get("text").and_then(Value::as_str)))?
                        .to_string(),
                ),
                "commandExecution" => CodexEvent::Tool(tool_summary("Bash", params.get("item")).0),
                "fileChange" => CodexEvent::Tool("Edit files".into()),
                "mcpToolCall" => CodexEvent::Tool(format!(
                    "{} · {}",
                    s("/item/server").unwrap_or_default(),
                    s("/item/tool").unwrap_or_default()
                )),
                "webSearch" => CodexEvent::Tool("Web search".into()),
                _ => return None,
            };
            seen(s("/threadId"), event)
        }
        "item/completed"
            if params.pointer("/item/type").and_then(Value::as_str) == Some("agentMessage") =>
        {
            seen(s("/threadId"), CodexEvent::Reply(s("/item/text")?))
        }
        _ => None,
    }
}

/// The id the observer reads the account's rate limits under.
const LIMITS_READ: i64 = 2;

/// Folds `from` into `into`: what it holds replaces what was known, and a
/// value it leaves out or sends as null keeps it.
fn fold(into: &mut Value, from: &Value) {
    match (into.as_object_mut(), from.as_object()) {
        (Some(into), Some(from)) => from
            .iter()
            .filter(|(_, v)| !v.is_null())
            .for_each(|(k, v)| match into.get_mut(k) {
                Some(known) if known.is_object() && v.is_object() => fold(known, v),
                _ => {
                    into.insert(k.clone(), v.clone());
                }
            }),
        _ => *into = from.clone(),
    }
}

/// The account's rate-limit snapshot with `msg` folded into `prev`: the
/// answer to `account/rateLimits/read`, or an `account/rateLimits/updated`
/// notice, which is sparse. `None` when `msg` holds no limits.
pub fn merge_limits(prev: Option<&Value>, msg: &Value) -> Option<Value> {
    let limits = msg.get("rateLimits").filter(|l| l.is_object())?;
    let mut merged = prev.cloned().unwrap_or_else(|| json!({}));
    fold(
        &mut merged,
        &json!({ "rateLimits": limits, "accountId": msg.get("accountId") }),
    );
    Some(merged)
}

/// Who a snapshot's account is, and its plan.
fn account_of(snapshot: &Value) -> MacAccount {
    let text = |p: &str| {
        snapshot
            .pointer(p)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    MacAccount {
        ids: text("/accountId").into_iter().collect(),
        plan: text("/rateLimits/planType").filter(|p| p != "unknown"),
    }
}

/// Joins the app-server as a second client until `cancel` fires or the
/// server goes away, forwarding what it sees, and the account's rate
/// limits to `plans`.
pub async fn observe(
    socket: PathBuf,
    events: mpsc::Sender<Seen>,
    plans: Option<Arc<dyn PlanSink>>,
    cancel: CancellationToken,
) -> Result<()> {
    let stream = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("connecting to {}", socket.display()))?;
    let (ws, _) = tokio_tungstenite::client_async("ws://localhost/", stream)
        .await
        .context("opening the app-server websocket")?;
    let (mut tx, mut rx) = ws.split();
    let init = json!({ "id": 1, "method": "initialize",
        "params": { "clientInfo": { "name": "workforce-desk", "version": env!("CARGO_PKG_VERSION") } } });
    tx.send(Message::text(init.to_string())).await?;
    tx.send(Message::text(
        json!({ "method": "initialized" }).to_string(),
    ))
    .await?;
    if plans.is_some() {
        let read = json!({ "id": LIMITS_READ, "method": "account/rateLimits/read",
            "params": { "excludeResetCreditDetails": true } });
        tx.send(Message::text(read.to_string())).await?;
    }
    let mut limits: Option<Value> = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            msg = rx.next() => {
                let Some(msg) = msg else { return Ok(()) };
                let Message::Text(text) = msg? else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                let reported = match (v.get("id"), v.get("method").and_then(Value::as_str)) {
                    (Some(id), None) if id.as_i64() == Some(LIMITS_READ) => v.get("result"),
                    (None, Some("account/rateLimits/updated")) => v.get("params"),
                    _ => None,
                };
                if let (Some(plans), Some(merged)) = (
                    &plans,
                    reported.and_then(|r| merge_limits(limits.as_ref(), r)),
                ) {
                    plans.mac(MacPlan::Codex, merged.clone(), account_of(&merged));
                    limits = Some(merged);
                }
                // Server requests (approvals) belong to the TUI; never answered here.
                if v.get("id").is_some() {
                    continue;
                }
                if let (Some(method), Some(params)) = (v.get("method").and_then(Value::as_str), v.get("params")) {
                    if let Some(seen) = interpret(method, params) {
                        events.send(seen).await?;
                    }
                }
            }
        }
    }
}

/// Codex keeps each conversation in `~/.codex/sessions/YYYY/MM/DD/rollout-…-<id>.jsonl`.
pub fn find_rollout(home: &Path, thread_id: &str) -> Option<PathBuf> {
    let suffix = format!("-{thread_id}.jsonl");
    let mut stack = vec![home.join(".codex/sessions")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&suffix))
            {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_reattaches_the_tui_to_the_session_app_server() {
        let env = BTreeMap::new();
        let s = spawn(&CodexSpawn {
            program: Path::new("/bin/codex"),
            cwd: Path::new("/repo"),
            socket: Path::new("/run/s.sock"),
            launch: Launch::Resume { thread_id: "t1" },
            prompt: None,
            env: &env,
        });
        assert_eq!(
            s.args,
            [
                "resume",
                "t1",
                "--remote",
                "unix:///run/s.sock",
                "-C",
                "/repo"
            ]
        );
    }

    #[test]
    fn notifications_become_card_state() {
        let started = |env: Value| json!({"thread": {"id": "t1", "ephemeral": false, "environments": env, "path": "/r/t1.jsonl"}});
        assert_eq!(
            interpret("thread/started", &started(json!([{"cwd": "/repo"}]))),
            Some(Seen {
                thread: "t1".into(),
                event: CodexEvent::Thread {
                    rollout: Some("/r/t1.jsonl".into())
                }
            })
        );
        assert_eq!(
            interpret("thread/started", &started(json!([]))),
            None,
            "Codex's own side thread"
        );
        assert_eq!(
            interpret(
                "thread/status/changed",
                &json!({"threadId": "t1", "status": {"type": "active", "activeFlags": ["waitingOnApproval"]}})
            ),
            Some(Seen {
                thread: "t1".into(),
                event: CodexEvent::Working { waiting: true }
            })
        );
        assert_eq!(
            interpret(
                "thread/status/changed",
                &json!({"threadId": "t1", "status": {"type": "idle"}})
            )
            .map(|s| s.event),
            Some(CodexEvent::Idle)
        );
        assert_eq!(
            interpret("item/started", &json!({"threadId": "t1", "item": {"type": "commandExecution", "command": "cargo test"}})).map(|s| s.event),
            Some(CodexEvent::Tool("Bash(cargo test)".into()))
        );
        assert_eq!(interpret("item/agentMessage/delta", &json!({})), None);
    }

    #[test]
    fn rollouts_are_found_by_thread_id() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join(".codex/sessions/2026/10/27");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-2026-10-27T17-06-33-t1.jsonl"), "").unwrap();
        assert!(find_rollout(home.path(), "t1").is_some());
        assert!(find_rollout(home.path(), "t2").is_none());
    }

    #[test]
    fn a_sparse_update_folds_into_the_last_read() {
        let read = json!({"accountId": "acct-1", "rateLimits": {"planType": "pro",
            "primary": {"usedPercent": 12, "windowDurationMins": 300, "resetsAt": 1793300000},
            "secondary": {"usedPercent": 40, "windowDurationMins": 10080, "resetsAt": 1793800000}}});
        let first = merge_limits(None, &read).unwrap();
        assert_eq!(
            account_of(&first),
            MacAccount {
                ids: vec!["acct-1".into()],
                plan: Some("pro".into())
            }
        );
        let update = json!({"rateLimits": {"planType": null,
            "primary": {"usedPercent": 15, "windowDurationMins": null, "resetsAt": 1793300000}}});
        let merged = merge_limits(Some(&first), &update).unwrap();
        assert_eq!(merged["rateLimits"]["primary"]["usedPercent"], 15);
        assert_eq!(
            merged["rateLimits"]["primary"]["windowDurationMins"], 300,
            "null keeps it"
        );
        assert_eq!(
            merged["rateLimits"]["secondary"]["usedPercent"], 40,
            "absent keeps it"
        );
        assert_eq!(merged["rateLimits"]["planType"], "pro");
        assert_eq!(merged["accountId"], "acct-1");
        assert_eq!(merge_limits(Some(&first), &json!({"other": 1})), None);
    }
}

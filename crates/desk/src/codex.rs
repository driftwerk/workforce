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
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::claude::tool_summary;
use crate::pty::Spawn;
use crate::usage::{LocalAccount, LocalPlan, PlanSink};

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

/// How long an app-server gets to wind down once told to stop, before it
/// is killed.
pub const STOP_GRACE: Duration = Duration::from_secs(5);
/// The same for what it left running (a worker outliving its server).
const WORKER_GRACE: Duration = Duration::from_secs(2);

/// The per-session app-server as the hub's own child.
pub struct AppServer {
    /// Until it is stopped: then a task reaps it.
    child: parking_lot::Mutex<Option<Child>>,
    pid: Option<u32>,
    pub socket: PathBuf,
}

impl AppServer {
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

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
            // Last resort, when the hub goes without stopping it.
            .kill_on_drop(true)
            .spawn()
            .context("starting codex app-server")?;
        wait_for_socket(&socket).await?;
        Ok(Self {
            pid: child.id(),
            child: parking_lot::Mutex::new(Some(child)),
            socket,
        })
    }

    /// Stops it and what it started, in the background (see
    /// [`stop_app_server`]); its exit is collected, so it leaves no zombie.
    pub fn stop(&self) {
        let (Some(pid), Some(mut child)) = (self.pid, self.child.lock().take()) else {
            return;
        };
        // Collects its exit as soon as it comes; killed through the handle
        // (which knows whether the number is still its own) when the
        // process list could not confirm it.
        let (unconfirmed, give_up) = oneshot::channel::<()>();
        let reaper = tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {}
                Ok(()) = give_up => {
                    let _ = child.kill().await;
                }
            }
        });
        let socket = self.socket.clone();
        tokio::spawn(async move {
            if stop_app_server(pid, &socket, STOP_GRACE).await == Stopped::Unconfirmed {
                let _ = unconfirmed.send(());
            }
            let _ = reaper.await;
        });
    }
}

fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only asks whether the process exists.
    (unsafe { libc::kill(pid, 0) } == 0)
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn signal(pid: u32, sig: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: a signal to a process id; one already gone is no error.
        unsafe { libc::kill(pid, sig) };
    }
}

/// Waits until none of `pids` runs, or `within` is up; those still running.
async fn wait_gone(mut pids: Vec<u32>, within: Duration) -> Vec<u32> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        pids.retain(|p| alive(*p));
        if pids.is_empty() || tokio::time::Instant::now() >= deadline {
            return pids;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Stops the app-server `pid` listening on `socket`, and everything it
/// started (Codex's workers run in process groups of their own, so a signal
/// to the server's does not reach them): a polite stop first, which lets a
/// turn in progress save its session, then a kill of whatever is still
/// running when `grace` is up. Each process is signalled by its own pid,
/// and only while it is still the one that was seen. Does nothing when
/// `pid` is no longer that server (it ended, and the number may be
/// someone else's by now): [`Stopped::Unconfirmed`], so a caller that owns
/// another way to end it can use that.
pub async fn stop_app_server(pid: u32, socket: &Path, grace: Duration) -> Stopped {
    let Some(all) = crate::procs::table().await else {
        tracing::warn!(
            pid,
            "codex app-server not stopped: the process list can't be read"
        );
        return Stopped::Unconfirmed;
    };
    let named = format!("unix://{}", socket.display());
    let is_server = |p: &crate::procs::Proc| {
        p.pid == pid && p.args.contains("app-server") && p.args.contains(&named)
    };
    if !all.iter().any(is_server) {
        return Stopped::Unconfirmed;
    }
    let mut below: Vec<crate::procs::Proc> = crate::procs::under(&all, pid)
        .into_iter()
        .cloned()
        .collect();
    signal(pid, libc::SIGTERM);
    if !wait_gone(vec![pid], grace).await.is_empty() {
        tracing::warn!(pid, "codex app-server did not stop in time; killing it");
        // The first look is up to `grace` old, and numbers are reused: of
        // what it showed, only what is still the same process is killed,
        // and whatever the server started since goes too.
        match crate::procs::table().await {
            Some(now) => {
                let same =
                    |b: &crate::procs::Proc| now.iter().any(|p| p.pid == b.pid && p.args == b.args);
                below.retain(same);
                for p in crate::procs::under(&now, pid) {
                    if !below.iter().any(|b| b.pid == p.pid) {
                        below.push(p.clone());
                    }
                }
                below.iter().for_each(|p| signal(p.pid, libc::SIGKILL));
                if now.iter().any(is_server) {
                    signal(pid, libc::SIGKILL);
                }
            }
            // Unreadable now: only the server, which is checked by its pid
            // having been the server a moment ago and still running.
            None => {
                below.clear();
                if alive(pid) {
                    signal(pid, libc::SIGKILL);
                }
            }
        }
        wait_gone(vec![pid], WORKER_GRACE).await;
    }
    // What outlived the server: told to stop, then killed. A process is
    // only touched while it is still the one that was seen.
    let seen = |now: &[crate::procs::Proc]| -> Vec<u32> {
        below
            .iter()
            .filter(|b| now.iter().any(|p| p.pid == b.pid && p.args == b.args))
            .map(|b| b.pid)
            .collect()
    };
    let Some(now) = crate::procs::table().await else {
        return Stopped::Done;
    };
    let left = seen(&now);
    if left.is_empty() {
        return Stopped::Done;
    }
    tracing::warn!(
        pid,
        left = left.len(),
        "codex app-server left workers running; stopping them"
    );
    left.iter().for_each(|p| signal(*p, libc::SIGTERM));
    if wait_gone(left.clone(), WORKER_GRACE).await.is_empty() {
        return Stopped::Done;
    }
    if let Some(now) = crate::procs::table().await {
        seen(&now).iter().for_each(|p| signal(*p, libc::SIGKILL));
    }
    Stopped::Done
}

/// What [`stop_app_server`] could tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// It was the server, and it and its workers were stopped.
    Done,
    /// The process list couldn't be read, or `pid` isn't the server: nothing
    /// was signalled.
    Unconfirmed,
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
    /// A sub-agent thread it started.
    Child(String),
    /// A model call's tokens, as the app-server reported them.
    Usage(crate::ledger::Spend),
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
        "thread/started"
            if params
                .pointer("/thread/parentThreadId")
                .is_some_and(|p| !p.is_null()) =>
        {
            seen(
                s("/thread/parentThreadId"),
                CodexEvent::Child(s("/thread/id")?),
            )
        }
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
        "thread/tokenUsage/updated" => seen(
            s("/threadId"),
            CodexEvent::Usage(crate::ledger::codex_spend(params, chrono::Utc::now())?),
        ),
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
fn account_of(snapshot: &Value) -> LocalAccount {
    let text = |p: &str| {
        snapshot
            .pointer(p)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    LocalAccount {
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
                    plans.local(LocalPlan::Codex, merged.clone(), account_of(&merged));
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
        // A folder that can't be read is skipped, not the whole search.
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
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

/// Every rollout Codex has, in one walk (a folder that can't be read is
/// skipped).
pub fn rollouts(home: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![home.join(".codex/sessions")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
            {
                found.push(path);
            }
        }
    }
    found
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
        assert_eq!(
            interpret(
                "thread/started",
                &json!({"thread": {"id": "t2", "parentThreadId": "t1", "environments": [{"cwd": "/repo"}]}})
            ),
            Some(Seen {
                thread: "t1".into(),
                event: CodexEvent::Child("t2".into())
            }),
            "a sub-agent's thread is its parent's"
        );
    }

    /// A script standing in for `codex app-server`: `body` runs once it has
    /// opened the socket path it was told to listen on.
    fn stand_in(dir: &Path, body: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let program = dir.join("fake-codex");
        std::fs::write(
            &program,
            format!("#!/bin/sh\n: > \"${{3#unix://}}\"\n{body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        (program, dir.join("s.sock"))
    }

    fn gone(pid: u32) -> bool {
        !alive(pid)
    }

    #[tokio::test]
    async fn a_server_that_winds_down_when_asked_is_not_killed() {
        let dir = tempfile::tempdir().unwrap();
        let said = dir.path().join("said");
        let (program, socket) = stand_in(
            dir.path(),
            &format!(
                "trap 'echo asked > \"{}\"; exit 0' TERM\nwhile :; do sleep 1; done",
                said.display()
            ),
        );
        let env = BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let server = AppServer::start(&program, socket.clone(), &env)
            .await
            .unwrap();
        let pid = server.pid().unwrap();
        let started = std::time::Instant::now();
        server.stop();
        let left = wait_gone(vec![pid], Duration::from_secs(4)).await;
        assert!(left.is_empty(), "still running");
        assert!(started.elapsed() < Duration::from_secs(4), "not waited out");
        assert!(said.exists(), "it was asked, and wound down itself");
    }

    #[tokio::test]
    async fn a_process_that_is_not_the_server_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut other = Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = other.id().unwrap();
        // The number was the server's once; it is someone else's now.
        let outcome =
            stop_app_server(pid, &dir.path().join("s.sock"), Duration::from_millis(200)).await;
        assert_eq!(outcome, Stopped::Unconfirmed);
        assert!(!gone(pid));
        other.kill().await.unwrap();
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
    fn a_folder_that_cant_be_read_does_not_end_the_search() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let sessions = home.path().join(".codex/sessions");
        let locked = sessions.join("2026/10/26");
        let open = sessions.join("2026/10/27");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::create_dir_all(&open).unwrap();
        std::fs::write(open.join("rollout-2026-10-27T17-06-33-t1.jsonl"), "").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let found = find_rollout(home.path(), "t1").is_some();
        let all = rollouts(home.path()).len();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(found);
        assert_eq!(all, 1);
    }

    #[test]
    fn a_sparse_update_folds_into_the_last_read() {
        let read = json!({"accountId": "acct-1", "rateLimits": {"planType": "pro",
            "primary": {"usedPercent": 12, "windowDurationMins": 300, "resetsAt": 1793300000},
            "secondary": {"usedPercent": 40, "windowDurationMins": 10080, "resetsAt": 1793800000}}});
        let first = merge_limits(None, &read).unwrap();
        assert_eq!(
            account_of(&first),
            LocalAccount {
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

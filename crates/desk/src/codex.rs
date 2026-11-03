//! Codex: the real `codex` TUI attached to an app-server the hub runs per
//! session (`--remote unix://…`). The hub joins the same app-server as a
//! second client and reads the session's state from its notifications, so
//! nothing is scraped off the screen. Subscribed to the session's thread
//! (`thread/resume`), it is asked what the TUI is asked, and answers the
//! command and file approvals the person answers in Desk; whoever answers
//! first, Desk or the terminal, wins (verified on CLI 0.166).

use std::collections::{BTreeMap, HashMap};
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
use crate::model::Decision;
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
    /// Codex's own reviewer answers its approvals.
    pub approve_for_me: bool,
    pub env: &'a BTreeMap<String, String>,
}

/// What `--approve-for-me` sets, as settings: approvals go to Codex's
/// automatic reviewer, in the workspace-write sandbox. Given to the TUI,
/// whose own settings start (and resume) its thread: on the app-server
/// alone they don't reach it (verified on CLI 0.166, `/status`).
pub const APPROVE_FOR_ME: [&str; 3] = [
    "approvals_reviewer=auto_review",
    "approval_policy=on-request",
    "sandbox_mode=workspace-write",
];

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
    if s.approve_for_me {
        args.extend(
            APPROVE_FOR_ME
                .iter()
                .flat_map(|c| ["-c".to_string(), c.to_string()]),
        );
    }
    args.extend(s.prompt.map(str::to_string));
    Spawn {
        program: s.program.to_path_buf(),
        args,
        cwd: s.cwd.to_path_buf(),
        env: s.env.clone(),
    }
}

/// The settings that give a session's app-server Desk's line ([`crate::line`])
/// as the MCP server `desk` at `url`: its tools run in the app-server, not
/// the TUI (`codex app-server -c key=value`, CLI 0.164). Its tools are
/// pre-approved, and a waiting `ask` (up to 300 s) isn't cut off.
pub fn line_settings(url: &str) -> [String; 3] {
    [
        format!("mcp_servers.desk.url={}", toml_string(url)),
        "mcp_servers.desk.tool_timeout_sec=330".to_string(),
        "mcp_servers.desk.default_tools_approval_mode=\"approve\"".to_string(),
    ]
}

/// `s` as a TOML basic string.
fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// How to run a session's app-server, listening on `socket`, with Desk's
/// line at `line` when it has one.
pub fn app_server(
    program: &Path,
    socket: &Path,
    env: &BTreeMap<String, String>,
    line: Option<&str>,
) -> Spawn {
    let mut args = vec![
        "app-server".to_string(),
        "--listen".into(),
        format!("unix://{}", socket.display()),
    ];
    for setting in line.map(line_settings).into_iter().flatten() {
        args.extend(["-c".to_string(), setting]);
    }
    Spawn {
        program: program.to_path_buf(),
        args,
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
        line: Option<&str>,
    ) -> Result<Self> {
        prepare_socket(&socket).await?;
        let spec = app_server(program, &socket, env, line);
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
                // What runs under the number now is only the server's while
                // the number is still the server.
                let still = now.iter().any(is_server);
                if still {
                    for p in crate::procs::under(&now, pid) {
                        if !below.iter().any(|b| b.pid == p.pid) {
                            below.push(p.clone());
                        }
                    }
                }
                below.iter().for_each(|p| signal(p.pid, libc::SIGKILL));
                if still {
                    signal(pid, libc::SIGKILL);
                }
            }
            // Unreadable now: nothing can be confirmed, so nothing is killed
            // here; the caller that owns another way to end it uses that.
            None => {
                tracing::warn!(
                    pid,
                    "codex app-server not killed: the process list can't be read"
                );
                return Stopped::Unconfirmed;
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
    /// It asks to run a command or change files, and Desk can answer.
    Asks(Ask),
    /// It asks the person something only its terminal answers: a question
    /// with its options (`item/tool/requestUserInput`), an MCP server's
    /// request for input. The request's JSON-RPC id.
    Question {
        request: Value,
        question: crate::claude::Question,
    },
    /// A request it asked was answered, by Desk or the terminal (the
    /// request's JSON-RPC id).
    Resolved(Value),
    /// Desk's answer to a request went to Codex, still open.
    Answered(Value),
    /// Desk's answer came after the request was answered (in the
    /// terminal): not sent.
    Late(Value),
    /// What Desk said into its running turn (`turn/steer`) didn't go in:
    /// no turn ran, or Codex refused it.
    SteerRefused,
    /// Who answers its thread's approvals, as Codex said when subscribed:
    /// `user`, or `auto_review` ("Approve for me").
    Reviewer(String),
    /// Its automatic reviewer ("Approve for me") decided a request: what
    /// it was, and the verdict in plain words.
    Reviewed {
        what: String,
        verdict: &'static str,
    },
}

/// A finished automatic review, as the activity line says it: `None` for
/// one that was called off.
fn reviewed(params: &Value) -> Option<CodexEvent> {
    let verdict = match params.pointer("/review/status").and_then(Value::as_str)? {
        "approved" => "allowed",
        "denied" => "denied",
        "timedOut" => "couldn't decide in time on",
        _ => return None,
    };
    let action = params.get("action")?;
    let what = match action.get("type").and_then(Value::as_str) {
        Some("command") => action
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string),
        Some("execve") => action
            .get("program")
            .and_then(Value::as_str)
            .map(str::to_string),
        Some("applyPatch") => action.get("files").and_then(Value::as_array).map(|f| {
            f.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        }),
        _ => None,
    }
    .filter(|w| !w.is_empty())
    .unwrap_or_else(|| "a request".into());
    Some(CodexEvent::Reviewed { what, verdict })
}

/// A command or file change Codex asks to be allowed.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    /// The request's JSON-RPC id, to answer it under.
    pub request: Value,
    /// `Bash` or `Edit`, as a Claude Code session's requests are named.
    pub tool: String,
    /// The command, or the files.
    pub detail: String,
    /// Codex's reason, and where it runs.
    pub why: Option<String>,
    /// What it can be answered with, of what Desk offers.
    pub choices: Vec<Decision>,
}

/// An answer to one of Codex's requests.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub request: Value,
    pub decision: Decision,
}

/// Codex's word for `d`, in an approval's answer.
pub fn codex_decision(d: Decision) -> Option<&'static str> {
    match d {
        Decision::Allow => Some("accept"),
        Decision::AllowForSession => Some("acceptForSession"),
        Decision::Deny => Some("decline"),
        Decision::Cancel => Some("cancel"),
        Decision::Terminal => None,
    }
}

/// What a request can be answered with: of what it lists, the plain
/// answers (a standing rule, the execpolicy and network amendments, is the
/// terminal's to write); all four when it lists none.
fn choices_of(params: &Value) -> Vec<Decision> {
    let Some(listed) = params.get("availableDecisions").and_then(Value::as_array) else {
        return vec![
            Decision::Allow,
            Decision::AllowForSession,
            Decision::Deny,
            Decision::Cancel,
        ];
    };
    listed
        .iter()
        .filter_map(|d| match d.as_str()? {
            "accept" => Some(Decision::Allow),
            "acceptForSession" => Some(Decision::AllowForSession),
            "decline" => Some(Decision::Deny),
            "cancel" => Some(Decision::Cancel),
            _ => None,
        })
        .collect()
}

/// `path` with the home folder as `~`.
fn tilde(path: &str, home: Option<&Path>) -> String {
    match home.and_then(|h| Path::new(path).strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.to_string(),
    }
}

/// "in ~/repo · Codex's reason".
fn why_of(params: &Value, home: Option<&Path>) -> Option<String> {
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(|c| format!("in {}", tilde(c, home)));
    let reason = params
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string);
    let said: Vec<String> = cwd.into_iter().chain(reason).collect();
    (!said.is_empty()).then(|| said.join(" · "))
}

/// The approval a server request is, when it is one Desk answers: a
/// command's, or a file change's (whose files its item named, in `files`).
pub fn ask_of(
    method: &str,
    id: &Value,
    params: &Value,
    files: &HashMap<String, Vec<String>>,
    home: Option<&Path>,
) -> Option<Seen> {
    let thread = params.get("threadId")?.as_str()?.to_string();
    let (tool, detail) = match method {
        "item/commandExecution/requestApproval" => {
            let actions: Vec<&str> = params
                .get("commandActions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|a| a.get("command").and_then(Value::as_str))
                .collect();
            let command = match actions.is_empty() {
                true => params.get("command").and_then(Value::as_str)?.to_string(),
                false => actions.join("; "),
            };
            ("Bash", command)
        }
        "item/fileChange/requestApproval" => {
            let item = params.get("itemId").and_then(Value::as_str)?;
            let named = files
                .get(item)
                .map(|f| {
                    f.iter()
                        .map(|p| tilde(p, home))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            ("Edit", named)
        }
        _ => return None,
    };
    Some(Seen {
        thread,
        event: CodexEvent::Asks(Ask {
            request: id.clone(),
            tool: tool.into(),
            detail,
            why: why_of(params, home),
            choices: choices_of(params),
        }),
    })
}

/// The question a server request puts to the person, when it is one only
/// the terminal answers: `item/tool/requestUserInput` that blocks the turn
/// (its first question, and its options' labels), or an MCP server's
/// `mcpServer/elicitation/request`. Shapes from 0.166's app-server schema.
pub fn question_of(method: &str, id: &Value, params: &Value) -> Option<Seen> {
    let thread = params.get("threadId")?.as_str()?.to_string();
    let text = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    let question = match method {
        // One the turn goes on past (`isBlocking: false`) doesn't wait.
        "item/tool/requestUserInput"
            if params.get("isBlocking").and_then(Value::as_bool) != Some(false) =>
        {
            let questions = params.get("questions")?.as_array()?;
            let first = questions.first()?;
            let detail = text(first, "question").or_else(|| text(first, "header"))?;
            let options: Vec<String> = first
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|o| text(o, "label"))
                .collect();
            crate::claude::Question {
                tool: "Question".into(),
                detail: crate::crew::clip(&detail, 300),
                why: (questions.len() > 1).then(|| format!("(1 of {} questions)", questions.len())),
                options: (!options.is_empty()).then_some(options),
            }
        }
        "mcpServer/elicitation/request" => {
            let said = text(params, "message")
                .or_else(|| text(params, "title"))
                .unwrap_or_else(|| "An MCP server needs your input".into());
            crate::claude::Question {
                tool: "Input".into(),
                detail: crate::crew::clip(&said, 300),
                why: text(params, "serverName").map(|s| format!("({s})")),
                options: None,
            }
        }
        _ => return None,
    };
    Some(Seen {
        thread,
        event: CodexEvent::Question {
            request: id.clone(),
            question,
        },
    })
}

/// What happened, and on which thread.
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    pub thread: String,
    pub event: CodexEvent,
}

/// Maps one app-server notification to what the session card shows.
/// Verified on CLI 0.164: `thread/started` and `thread/status/changed`
/// reach every client; item notifications, and the thread's requests, only
/// reach its subscribers (0.166).
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
        "item/autoApprovalReview/completed" => seen(s("/threadId"), reviewed(params)?),
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
/// The id it subscribes to the session's thread under.
const SUBSCRIBE: i64 = 3;
/// The id it steers the session's turn under.
const STEER: i64 = 4;

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

/// What the observer knows of the session's thread, and what it does
/// with each message: kept apart from the socket, to be tested alone.
pub struct Follower {
    /// The session's thread: known at start when it is resumed or taken
    /// back, else the first conversation it starts.
    thread: Option<String>,
    /// Subscribed to it, or asking to be.
    subscribed: bool,
    /// Subscribes as soon as it joins: a session taken back, whose TUI
    /// already has its thread. Otherwise at the thread's first status, so
    /// the TUI loads it first, with its own settings.
    at_once: bool,
    /// The files each file change item it started touches, by item id.
    files: HashMap<String, Vec<String>>,
    /// The requests it forwarded that nobody has answered, by id.
    open: HashMap<String, String>,
    home: Option<PathBuf>,
    /// The thread's turn running now, which a steer must name.
    turn: Option<String>,
}

impl Follower {
    pub fn new(thread: Option<String>, home: Option<PathBuf>) -> Self {
        Self {
            thread,
            subscribed: false,
            at_once: false,
            files: HashMap::new(),
            open: HashMap::new(),
            home,
            turn: None,
        }
    }

    /// Subscribes as soon as it joins (see `at_once`).
    pub fn at_once(mut self) -> Self {
        self.at_once = true;
        self
    }

    /// Subscribes to the thread, when it is known and not yet subscribed.
    /// Without, it hears only the thread's status, never its requests.
    fn subscribe(&mut self) -> Option<Value> {
        let thread = self.thread.as_ref().filter(|_| !self.subscribed)?;
        self.subscribed = true;
        // Not its history, which can outgrow a frame: what it asks still
        // comes.
        Some(json!({ "id": SUBSCRIBE, "method": "thread/resume",
            "params": { "threadId": thread, "excludeTurns": true } }))
    }

    /// What to send once joined.
    pub fn joined(&mut self) -> Option<Value> {
        // Otherwise at its first status.
        self.at_once.then(|| self.subscribe()).flatten()
    }

    /// What `v` says, and what to send for it. A server request other than
    /// a command's or a file change's approval is never answered here,
    /// not even with an error: the first answer wins, and it is the TUI's.
    pub fn heard(&mut self, v: &Value) -> (Vec<Seen>, Option<Value>) {
        let method = v.get("method").and_then(Value::as_str);
        match (v.get("id"), method) {
            (Some(id), None) => {
                // The thread can't be resumed before its first turn has
                // written it down ("no rollout found"): again at its next
                // status.
                // A steer Codex refused (its turn had ended): what was
                // said goes in at the next idle instead.
                if id.as_i64() == Some(STEER) {
                    let refused = v.get("error").and(self.thread.clone()).map(|thread| Seen {
                        thread,
                        event: CodexEvent::SteerRefused,
                    });
                    return (refused.into_iter().collect(), None);
                }
                if id.as_i64() != Some(SUBSCRIBE) {
                    return (Vec::new(), None);
                }
                if v.get("error").is_some() {
                    tracing::debug!(error = %v["error"], "codex: not subscribed to the thread yet");
                    self.subscribed = false;
                    return (Vec::new(), None);
                }
                // What the thread runs with, as Codex says it.
                let said = v
                    .pointer("/result/approvalsReviewer")
                    .and_then(Value::as_str)
                    .zip(self.thread.clone())
                    .map(|(reviewer, thread)| Seen {
                        thread,
                        event: CodexEvent::Reviewer(reviewer.to_string()),
                    });
                (said.into_iter().collect(), None)
            }
            (Some(id), Some(method)) => {
                let params = v.get("params").cloned().unwrap_or(Value::Null);
                let asks = ask_of(method, id, &params, &self.files, self.home.as_deref())
                    .filter(|s| self.thread.as_deref() == Some(s.thread.as_str()));
                if let Some(s) = &asks {
                    self.open.insert(id.to_string(), s.thread.clone());
                }
                // A question is the terminal's to answer: only shown.
                let asked = asks
                    .is_none()
                    .then(|| question_of(method, id, &params))
                    .flatten()
                    .filter(|s| self.thread.as_deref() == Some(s.thread.as_str()));
                (asks.into_iter().chain(asked).collect(), None)
            }
            (None, Some(method)) => {
                let params = v.get("params").cloned().unwrap_or(Value::Null);
                if method == "item/started"
                    && params.pointer("/item/type").and_then(Value::as_str) == Some("fileChange")
                {
                    if let Some(item) = params.pointer("/item/id").and_then(Value::as_str) {
                        let paths = params
                            .pointer("/item/changes")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|c| c.get("path").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect();
                        self.files.insert(item.to_string(), paths);
                    }
                }
                let ours = params.get("threadId").and_then(Value::as_str).is_some()
                    && params.get("threadId").and_then(Value::as_str) == self.thread.as_deref();
                match method {
                    "turn/started" if ours => {
                        self.turn = params
                            .pointer("/turn/id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    "turn/completed" if ours => self.turn = None,
                    _ => {}
                }
                if method == "serverRequest/resolved" {
                    if let Some(request) = params.get("requestId") {
                        self.open.remove(&request.to_string());
                    }
                    let seen = params
                        .get("threadId")
                        .and_then(Value::as_str)
                        .zip(params.get("requestId"))
                        .map(|(thread, request)| Seen {
                            thread: thread.to_string(),
                            event: CodexEvent::Resolved(request.clone()),
                        });
                    return (seen.into_iter().collect(), None);
                }
                let seen = interpret(method, &params);
                if let Some(Seen {
                    thread,
                    event: CodexEvent::Thread { .. },
                }) = &seen
                {
                    self.thread.get_or_insert_with(|| thread.clone());
                }
                let send = match (method, params.get("threadId").and_then(Value::as_str)) {
                    ("thread/status/changed", Some(t)) if self.thread.as_deref() == Some(t) => {
                        self.subscribe()
                    }
                    _ => None,
                };
                (seen.into_iter().collect(), send)
            }
            (None, None) => (Vec::new(), None),
        }
    }
}

impl Follower {
    /// What to send to steer the thread's running turn with `text`
    /// (`turn/steer`, which must name the running turn: verified in the
    /// 0.166 schema); nothing while no turn runs.
    pub fn steer(&self, text: &str) -> Option<Value> {
        let (thread, turn) = (self.thread.as_ref()?, self.turn.as_ref()?);
        Some(json!({ "id": STEER, "method": "turn/steer", "params": {
            "threadId": thread, "expectedTurnId": turn,
            "input": [{ "type": "text", "text": text }] } }))
    }

    /// What to send for `reply`, and what to say of it: nothing is sent to
    /// a request already answered.
    pub fn reply(&mut self, reply: &Reply) -> (Option<Seen>, Option<Value>) {
        let thread = self.open.remove(&reply.request.to_string());
        let answer = answer_of(reply).filter(|_| thread.is_some());
        let event = match answer {
            Some(_) => CodexEvent::Answered(reply.request.clone()),
            None => CodexEvent::Late(reply.request.clone()),
        };
        let thread = thread.or_else(|| self.thread.clone());
        (thread.map(|thread| Seen { thread, event }), answer)
    }
}

/// The answer to send for `reply`.
fn answer_of(reply: &Reply) -> Option<Value> {
    let decision = codex_decision(reply.decision)?;
    Some(json!({ "id": reply.request, "result": { "decision": decision } }))
}

/// Joins the app-server as a second client until `cancel` fires or the
/// server goes away, forwarding what it sees, and the account's rate
/// limits to `plans`; subscribed to `thread` (else the first it starts),
/// it forwards what it is asked, and sends `replies`.
pub async fn observe(
    socket: PathBuf,
    mut follower: Follower,
    events: mpsc::Sender<Seen>,
    mut replies: mpsc::UnboundedReceiver<Reply>,
    mut steers: mpsc::UnboundedReceiver<String>,
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
    if let Some(subscribe) = follower.joined() {
        tx.send(Message::text(subscribe.to_string())).await?;
    }
    let mut limits: Option<Value> = None;
    let mut replying = true;
    let mut steering = true;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            steer = steers.recv(), if steering => match steer {
                Some(text) => {
                    match follower.steer(&text) {
                        Some(send) => tx.send(Message::text(send.to_string())).await?,
                        // No turn runs: said at the next idle instead.
                        None => {
                            if let Some(thread) = follower.thread.clone() {
                                events.send(Seen { thread, event: CodexEvent::SteerRefused }).await?;
                            }
                        }
                    }
                }
                None => steering = false,
            },
            reply = replies.recv(), if replying => match reply {
                Some(reply) => {
                    let (seen, answer) = follower.reply(&reply);
                    if let Some(answer) = answer {
                        tx.send(Message::text(answer.to_string())).await?;
                    }
                    if let Some(seen) = seen {
                        events.send(seen).await?;
                    }
                }
                None => replying = false,
            },
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
                let (seen, send) = follower.heard(&v);
                if let Some(send) = send {
                    tx.send(Message::text(send.to_string())).await?;
                }
                for seen in seen {
                    events.send(seen).await?;
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
pub(crate) mod tests {
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
            approve_for_me: false,
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
    fn approve_for_me_is_the_tuis_to_say() {
        let env = BTreeMap::new();
        for launch in [Launch::New, Launch::Resume { thread_id: "t1" }] {
            let s = spawn(&CodexSpawn {
                program: Path::new("/bin/codex"),
                cwd: Path::new("/repo"),
                socket: Path::new("/run/s.sock"),
                launch,
                prompt: Some("go"),
                approve_for_me: true,
                env: &env,
            });
            let tail: Vec<&str> = s
                .args
                .iter()
                .rev()
                .take(7)
                .rev()
                .map(String::as_str)
                .collect();
            assert_eq!(
                tail,
                [
                    "-c",
                    "approvals_reviewer=auto_review",
                    "-c",
                    "approval_policy=on-request",
                    "-c",
                    "sandbox_mode=workspace-write",
                    "go"
                ]
            );
        }
    }

    #[test]
    fn its_reviewers_verdicts_are_said_plainly() {
        let done = |status: &str, action: Value| {
            interpret(
                "item/autoApprovalReview/completed",
                &json!({"threadId": "t1", "turnId": "u1", "reviewId": "r1", "startedAtMs": 1,
                    "completedAtMs": 2, "decisionSource": "agent",
                    "review": {"status": status}, "action": action}),
            )
            .map(|s| s.event)
        };
        let rm =
            json!({"type": "command", "command": "rm a.txt", "cwd": "/repo", "source": "shell"});
        assert_eq!(
            done("approved", rm.clone()),
            Some(CodexEvent::Reviewed {
                what: "rm a.txt".into(),
                verdict: "allowed"
            })
        );
        assert_eq!(
            done(
                "denied",
                json!({"type": "applyPatch", "cwd": "/repo", "files": ["a.rs", "b.rs"]})
            ),
            Some(CodexEvent::Reviewed {
                what: "a.rs, b.rs".into(),
                verdict: "denied"
            })
        );
        assert_eq!(done("aborted", rm.clone()), None);
        assert_eq!(done("inProgress", rm), None);
    }

    #[test]
    fn a_steer_names_the_running_turn_and_waits_for_one() {
        let mut f = Follower::new(Some("t1".into()), None);
        assert!(f.steer("stop").is_none(), "no turn runs");
        f.heard(
            &json!({"method": "turn/started", "params": {"threadId": "t1", "turn": {"id": "u9"}}}),
        );
        f.heard(&json!({"method": "turn/started", "params": {"threadId": "other", "turn": {"id": "x"}}}));
        assert_eq!(
            f.steer("stop").unwrap(),
            json!({"id": STEER, "method": "turn/steer", "params": {"threadId": "t1",
                "expectedTurnId": "u9", "input": [{"type": "text", "text": "stop"}]}})
        );
        f.heard(&json!({"method": "turn/completed", "params": {"threadId": "t1", "turn": {"id": "u9"}}}));
        assert!(f.steer("stop").is_none());
    }

    #[test]
    fn a_steer_codex_refuses_is_said_so_and_one_it_takes_is_not() {
        let mut f = Follower::new(Some("t1".into()), None);
        let (seen, _) =
            f.heard(&json!({"id": STEER, "error": {"code": -32600, "message": "no active turn"}}));
        assert!(matches!(
            seen.as_slice(),
            [Seen {
                event: CodexEvent::SteerRefused,
                ..
            }]
        ));
        let (seen, _) = f.heard(&json!({"id": STEER, "result": {}}));
        assert!(seen.is_empty());
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
        let server = AppServer::start(&program, socket.clone(), &env, None)
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

    /// The escalated command a spike on CLI 0.166 was asked, as it came.
    fn escalated() -> Value {
        json!({"kind": "command", "threadId": "t1", "turnId": "u1", "itemId": "exec-1",
            "startedAtMs": 1793635270599_i64, "environmentId": "local",
            "reason": "Do you want to allow deleting victim.txt?",
            "command": "/bin/zsh -lc 'rm victim.txt'", "cwd": "/Users/k/repo",
            "commandActions": [{"type": "unknown", "command": "rm victim.txt"}],
            "proposedExecpolicyAmendment": ["rm", "victim.txt"],
            "availableDecisions": ["accept",
                {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["rm", "victim.txt"]}},
                "cancel"]})
    }

    #[test]
    fn a_command_request_offers_what_codex_does_without_standing_rules() {
        let home = Path::new("/Users/k");
        let seen = ask_of(
            "item/commandExecution/requestApproval",
            &json!(0),
            &escalated(),
            &HashMap::new(),
            Some(home),
        )
        .unwrap();
        assert_eq!(seen.thread, "t1");
        assert_eq!(
            seen.event,
            CodexEvent::Asks(Ask {
                request: json!(0),
                tool: "Bash".into(),
                detail: "rm victim.txt".into(),
                why: Some("in ~/repo · Do you want to allow deleting victim.txt?".into()),
                choices: vec![Decision::Allow, Decision::Cancel],
            }),
            "the amendment writes a rule: the terminal's to offer"
        );
        let mut plain = escalated();
        plain.as_object_mut().unwrap().remove("availableDecisions");
        plain.as_object_mut().unwrap().remove("commandActions");
        let Some(CodexEvent::Asks(ask)) = ask_of(
            "item/commandExecution/requestApproval",
            &json!(4),
            &plain,
            &HashMap::new(),
            None,
        )
        .map(|s| s.event) else {
            panic!("an ask")
        };
        assert_eq!(ask.detail, "/bin/zsh -lc 'rm victim.txt'");
        assert_eq!(
            ask.choices,
            [
                Decision::Allow,
                Decision::AllowForSession,
                Decision::Deny,
                Decision::Cancel
            ],
            "unlisted: all four"
        );
        for other in [
            "item/tool/requestUserInput",
            "mcpServer/elicitation/request",
            "execCommandApproval",
        ] {
            assert_eq!(
                ask_of(other, &json!(1), &escalated(), &HashMap::new(), None),
                None
            );
        }
    }

    #[test]
    fn a_question_or_an_mcp_request_is_the_terminal_s_to_answer() {
        let mut f = Follower::new(Some("t1".into()), None);
        let (seen, send) = f.heard(&json!({"id": 9, "method": "mcpServer/elicitation/request",
            "params": {"threadId": "t1", "serverName": "db", "mode": "form",
                "message": "Which database?", "requestedSchema": {"type": "object", "properties": {}}}}));
        assert_eq!(send, None, "never answered from Desk");
        let [Seen {
            event: CodexEvent::Question { request, question },
            ..
        }] = &seen[..]
        else {
            panic!("{seen:?}")
        };
        assert_eq!(
            (
                request,
                question.tool.as_str(),
                question.detail.as_str(),
                question.why.as_deref()
            ),
            (&json!(9), "Input", "Which database?", Some("(db)"))
        );
        let (seen, _) = f.heard(&json!({"id": 10, "method": "item/tool/requestUserInput",
            "params": {"threadId": "other", "turnId": "u", "itemId": "i", "isBlocking": true,
                "questions": [{"id": "a", "header": "H", "question": "Q?"}]}}));
        assert!(seen.is_empty(), "another thread's");
    }

    #[test]
    fn a_file_change_request_names_the_files_its_item_did() {
        let mut f = Follower::new(Some("t1".into()), Some(PathBuf::from("/Users/k")));
        f.heard(
            &json!({"method": "item/started", "params": {"threadId": "t1",
            "item": {"type": "fileChange", "id": "fc-1", "status": "inProgress",
                "changes": [{"path": "/Users/k/repo/a.rs", "kind": "update", "diff": ""},
                            {"path": "/etc/hosts", "kind": "update", "diff": ""}]}}}),
        );
        let (seen, send) = f.heard(
            &json!({"id": 7, "method": "item/fileChange/requestApproval",
            "params": {"threadId": "t1", "turnId": "u1", "itemId": "fc-1", "startedAtMs": 1,
                "reason": "needs write access"}}),
        );
        assert_eq!(
            send, None,
            "a request is answered only when the person does"
        );
        let [Seen {
            event: CodexEvent::Asks(ask),
            ..
        }] = &seen[..]
        else {
            panic!("{seen:?}")
        };
        assert_eq!(
            (ask.tool.as_str(), ask.detail.as_str()),
            ("Edit", "~/repo/a.rs, /etc/hosts")
        );
        assert_eq!(ask.why.as_deref(), Some("needs write access"));
        assert_eq!(ask.request, json!(7));
    }

    #[test]
    fn it_subscribes_to_its_thread_once_it_has_one_and_again_when_refused() {
        let mut f = Follower::new(None, None);
        assert_eq!(f.joined(), None, "no thread yet");
        let active = json!({"method": "thread/status/changed",
            "params": {"threadId": "t1", "status": {"type": "active", "activeFlags": []}}});
        assert_eq!(f.heard(&active).1, None, "not its thread yet");
        f.heard(
            &json!({"method": "thread/started", "params": {"thread": {"id": "t1",
            "environments": [{"cwd": "/repo"}]}}}),
        );
        let other = json!({"method": "thread/status/changed",
            "params": {"threadId": "side", "status": {"type": "active", "activeFlags": []}}});
        assert_eq!(f.heard(&other).1, None, "only its own thread");
        let resume = json!({"id": SUBSCRIBE, "method": "thread/resume", "params": {"threadId": "t1", "excludeTurns": true}});
        assert_eq!(f.heard(&active).1, Some(resume.clone()));
        assert_eq!(f.heard(&active).1, None, "asked once");
        f.heard(
            &json!({"id": SUBSCRIBE, "error": {"code": -32600, "message": "no rollout found"}}),
        );
        assert_eq!(
            f.heard(&active).1,
            Some(resume),
            "again, at its next status"
        );
        // Taken back: its thread is known, and it subscribes at once.
        let mut launched = Follower::new(Some("t9".into()), None);
        assert_eq!(
            launched.joined(),
            None,
            "resumed: once its TUI has loaded it"
        );
        let status = json!({"method": "thread/status/changed",
            "params": {"threadId": "t9", "status": {"type": "idle"}}});
        assert!(launched.heard(&status).1.is_some());
        let (seen, _) = launched
            .heard(&json!({"id": SUBSCRIBE, "result": {"approvalsReviewer": "auto_review"}}));
        assert_eq!(
            seen,
            [Seen {
                thread: "t9".into(),
                event: CodexEvent::Reviewer("auto_review".into())
            }]
        );
        let mut back = Follower::new(Some("t9".into()), None).at_once();
        assert_eq!(
            back.joined(),
            Some(
                json!({"id": SUBSCRIBE, "method": "thread/resume", "params": {"threadId": "t9", "excludeTurns": true}})
            )
        );
    }

    #[test]
    fn requests_of_another_thread_and_resolutions_are_told_apart() {
        let mut f = Follower::new(Some("t1".into()), None);
        let mut child = escalated();
        child["threadId"] = json!("child");
        let (seen, _) = f.heard(
            &json!({"id": 0, "method": "item/commandExecution/requestApproval", "params": child}),
        );
        assert!(seen.is_empty(), "a sub-agent's asks stay its terminal's");
        let (seen, _) = f.heard(&json!({"method": "serverRequest/resolved",
            "params": {"threadId": "t1", "requestId": 0}}));
        assert_eq!(
            seen,
            [Seen {
                thread: "t1".into(),
                event: CodexEvent::Resolved(json!(0))
            }]
        );
        assert_eq!(
            answer_of(&Reply {
                request: json!(0),
                decision: Decision::AllowForSession
            }),
            Some(json!({"id": 0, "result": {"decision": "acceptForSession"}}))
        );
        assert_eq!(
            answer_of(&Reply {
                request: json!(0),
                decision: Decision::Terminal
            }),
            None
        );
    }

    /// A stand-in app-server on a socket of its own: what each test does
    /// with the connection the observer opens.
    pub(crate) async fn fake_server(
        socket: &Path,
    ) -> tokio::task::JoinHandle<tokio_tungstenite::WebSocketStream<UnixStream>> {
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_async(stream).await.unwrap()
        })
    }

    /// The next message the observer sends whose `method` (or answer) is
    /// `what`, skipping the rest.
    pub(crate) async fn next_sent(
        ws: &mut tokio_tungstenite::WebSocketStream<UnixStream>,
        what: impl Fn(&Value) -> bool,
    ) -> Value {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("in time")
                .expect("open")
                .unwrap();
            if let Message::Text(t) = msg {
                let v: Value = serde_json::from_str(&t).unwrap();
                if what(&v) {
                    return v;
                }
            }
        }
    }

    pub(crate) async fn say(ws: &mut tokio_tungstenite::WebSocketStream<UnixStream>, v: Value) {
        ws.send(Message::text(v.to_string())).await.unwrap();
    }

    #[tokio::test]
    async fn the_observer_hears_what_codex_asks_and_answers_it() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        let server = fake_server(&socket).await;
        let (events, mut seen) = mpsc::channel(16);
        let (replies, answers) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let observer = tokio::spawn(observe(
            socket,
            Follower::new(Some("t1".into()), None).at_once(),
            events,
            answers,
            mpsc::unbounded_channel().1,
            None,
            cancel.clone(),
        ));
        let mut ws = server.await.unwrap();
        let method = |m: &'static str| move |v: &Value| v["method"] == m;
        next_sent(&mut ws, method("initialize")).await;
        let resume = next_sent(&mut ws, method("thread/resume")).await;
        assert_eq!(resume["params"]["threadId"], "t1");
        say(
            &mut ws,
            json!({"id": resume["id"], "result": {"thread": {"id": "t1"}}}),
        )
        .await;
        say(&mut ws, json!({"id": 0, "method": "item/commandExecution/requestApproval", "params": escalated()})).await;
        // Something it is never to answer: the TUI's.
        say(
            &mut ws,
            json!({"id": 1, "method": "item/tool/requestUserInput", "params": {"threadId": "t1"}}),
        )
        .await;
        let got = tokio::time::timeout(Duration::from_secs(5), seen.recv())
            .await
            .unwrap()
            .unwrap();
        let CodexEvent::Asks(ask) = got.event else {
            panic!("{got:?}")
        };
        replies
            .send(Reply {
                request: ask.request,
                decision: Decision::Allow,
            })
            .unwrap();
        let answer = next_sent(&mut ws, |v| v.get("result").is_some()).await;
        assert_eq!(answer, json!({"id": 0, "result": {"decision": "accept"}}));
        macro_rules! next {
            () => {
                tokio::time::timeout(Duration::from_secs(5), seen.recv())
                    .await
                    .unwrap()
                    .unwrap()
            };
        }
        assert_eq!(next!().event, CodexEvent::Answered(json!(0)));
        say(&mut ws, json!({"method": "serverRequest/resolved", "params": {"threadId": "t1", "requestId": 0}})).await;
        assert_eq!(next!().event, CodexEvent::Resolved(json!(0)));
        // Answered again, after: not sent, and said so.
        replies
            .send(Reply {
                request: json!(0),
                decision: Decision::Allow,
            })
            .unwrap();
        assert_eq!(next!().event, CodexEvent::Late(json!(0)));
        // The server goes (without a goodbye): so does the observer, and
        // what it forwards.
        drop(ws);
        let _ = tokio::time::timeout(Duration::from_secs(5), observer)
            .await
            .unwrap()
            .unwrap();
        assert!(seen.recv().await.is_none());
    }
}

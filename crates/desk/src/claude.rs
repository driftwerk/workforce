//! Claude Code: the real interactive `claude` TUI, with per-spawn hooks that
//! report its state to the hub. Nothing is written to the user's settings.
//!
//! Verified on CLI 2.1.310 in a PTY: `http` hooks fire for every event except
//! SessionStart, so SessionStart is a `command` hook posting the same JSON.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::background::TaskKind;
use crate::pty::Spawn;

/// What to run: a new conversation with an id Desk chose, or one to resume.
pub enum Launch<'a> {
    New { session_id: &'a str },
    Resume { session_id: &'a str },
}

pub struct ClaudeSpawn<'a> {
    pub program: &'a Path,
    pub cwd: &'a Path,
    pub name: &'a str,
    pub launch: Launch<'a>,
    pub prompt: Option<&'a str>,
    pub hook_url: &'a str,
    pub approval_timeout_secs: u64,
    pub env: &'a BTreeMap<String, String>,
}

const TOOL_EVENTS: [&str; 3] = ["PreToolUse", "PermissionRequest", "PostToolUse"];
const PLAIN_EVENTS: [&str; 6] = [
    "UserPromptSubmit",
    "Notification",
    "Stop",
    "SubagentStart",
    "SubagentStop",
    "SessionEnd",
];

fn http_hook(url: &str, timeout_secs: u64) -> Value {
    json!({ "type": "http", "url": url, "timeout": timeout_secs })
}

pub fn hook_settings(hook_url: &str, approval_timeout_secs: u64) -> Value {
    let mut hooks = serde_json::Map::new();
    let post = format!(
        "curl -s --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary @- '{hook_url}'"
    );
    hooks.insert(
        "SessionStart".into(),
        json!([{ "hooks": [{ "type": "command", "command": post, "timeout": 10 }] }]),
    );
    for event in TOOL_EVENTS {
        let timeout = if event == "PermissionRequest" {
            approval_timeout_secs
        } else {
            10
        };
        hooks.insert(
            event.into(),
            json!([{ "matcher": "*", "hooks": [http_hook(hook_url, timeout)] }]),
        );
    }
    for event in PLAIN_EVENTS {
        hooks.insert(
            event.into(),
            json!([{ "hooks": [http_hook(hook_url, 10)] }]),
        );
    }
    json!({ "hooks": hooks })
}

pub fn spawn(s: &ClaudeSpawn<'_>) -> Spawn {
    let mut args = match s.launch {
        Launch::New { session_id } => vec!["--session-id".to_string(), session_id.to_string()],
        Launch::Resume { session_id } => vec!["--resume".to_string(), session_id.to_string()],
    };
    args.extend([
        "-n".to_string(),
        s.name.to_string(),
        "--settings".to_string(),
        hook_settings(s.hook_url, s.approval_timeout_secs).to_string(),
    ]);
    args.extend(s.prompt.map(str::to_string));
    let mut env = s.env.clone();
    env.insert(USAGE_URL.into(), s.hook_url.to_string());
    Spawn {
        program: s.program.to_path_buf(),
        args,
        cwd: s.cwd.to_path_buf(),
        env,
    }
}

/// Where Desk's plugin posts the plan's rate limits from.
pub const USAGE_URL: &str = "DESK_USAGE_URL";

#[derive(Debug, Deserialize)]
struct Payload {
    hook_event_name: String,
    session_id: Option<String>,
    transcript_path: Option<String>,
    source: Option<String>,
    prompt: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    notification_type: Option<String>,
    message: Option<String>,
    last_assistant_message: Option<String>,
    rate_limits: Option<Value>,
    agent_id: Option<String>,
    background_tasks: Option<Vec<BackgroundTask>>,
}

/// One entry of a `Stop`'s `background_tasks`.
#[derive(Debug, Deserialize)]
struct BackgroundTask {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    status: String,
}

/// The prompts Claude Code submits itself when background work it started
/// ends: not the person's words.
const ENGINE_PROMPTS: [&str; 2] = ["<task-notification>", "<agent-message from="];

#[derive(Debug, Clone, PartialEq)]
pub enum Hook {
    SessionStart {
        session_id: Option<String>,
        transcript: Option<String>,
        resumed: bool,
    },
    Prompt(String),
    ToolStart(String),
    Permission {
        tool: String,
        detail: String,
    },
    ToolEnd,
    NeedsInput(String),
    Stop {
        transcript: Option<String>,
        reply: Option<String>,
        /// What it left running, when the CLI says.
        background: Option<Vec<(String, TaskKind)>>,
    },
    /// Claude Code went on by itself: background work it started ended.
    Woken,
    SubagentStart(String),
    SubagentStop(String),
    SessionEnd,
    /// Desk's plugin reporting the plan's rate limits (`DeskUsage`).
    Usage(Value),
    Other,
}

pub fn parse_hook(body: &[u8]) -> eyre::Result<Hook> {
    let p: Payload = serde_json::from_slice(body)?;
    let tool = || {
        tool_summary(
            p.tool_name.as_deref().unwrap_or("tool"),
            p.tool_input.as_ref(),
        )
    };
    Ok(match p.hook_event_name.as_str() {
        "SessionStart" => Hook::SessionStart {
            session_id: p.session_id,
            transcript: p.transcript_path,
            resumed: p.source.as_deref() == Some("resume"),
        },
        "UserPromptSubmit" => match p.prompt.unwrap_or_default() {
            prompt
                if ENGINE_PROMPTS
                    .iter()
                    .any(|e| prompt.trim_start().starts_with(e)) =>
            {
                Hook::Woken
            }
            prompt => Hook::Prompt(prompt),
        },
        "PreToolUse" => Hook::ToolStart(tool().0),
        "PermissionRequest" => {
            let (tool, detail) = tool();
            Hook::Permission { tool, detail }
        }
        "PostToolUse" => Hook::ToolEnd,
        "Notification" if p.notification_type.as_deref() != Some("idle_prompt") => {
            Hook::NeedsInput(p.message.unwrap_or_default())
        }
        "Stop" => Hook::Stop {
            transcript: p.transcript_path,
            reply: p.last_assistant_message.filter(|m| !m.trim().is_empty()),
            background: p.background_tasks.map(|tasks| {
                tasks
                    .into_iter()
                    .filter(|t| t.status == "running")
                    .map(|t| {
                        let kind = match t.kind.as_str() {
                            "shell" => TaskKind::Shell,
                            _ => TaskKind::Agent,
                        };
                        (t.id, kind)
                    })
                    .collect()
            }),
        },
        "SubagentStart" => p.agent_id.map_or(Hook::Other, Hook::SubagentStart),
        "SubagentStop" => p.agent_id.map_or(Hook::Other, Hook::SubagentStop),
        "SessionEnd" => Hook::SessionEnd,
        "DeskUsage" => Hook::Usage(p.rate_limits.unwrap_or(Value::Null)),
        _ => Hook::Other,
    })
}

/// The sub-agent a hook fired inside, if it fired inside one.
pub fn agent_of(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Payload>(body).ok()?.agent_id
}

const DETAIL_MAX: usize = 160;

fn clip(s: &str) -> String {
    let line = s.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= DETAIL_MAX {
        line.to_string()
    } else {
        format!("{}…", line.chars().take(DETAIL_MAX).collect::<String>())
    }
}

/// A short label for the tool (`Bash`, `Edit`, `workforce · memory.recall`)
/// and the detail a person needs to approve it (the command, the file).
pub fn tool_summary(name: &str, input: Option<&Value>) -> (String, String) {
    let field = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str);
    let label = match name.strip_prefix("mcp__") {
        Some(rest) => rest.replacen("__", " · ", 1),
        None => name.to_string(),
    };
    let detail = field("command")
        .or_else(|| field("file_path"))
        .or_else(|| field("path"))
        .or_else(|| field("url"))
        .or_else(|| field("pattern"))
        .map(clip)
        .unwrap_or_default();
    let shown = if detail.is_empty() {
        label.clone()
    } else {
        format!("{label}({detail})")
    };
    (clip(&shown), detail)
}

pub fn allow() -> Value {
    json!({ "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": { "behavior": "allow" } } })
}

pub fn deny() -> Value {
    json!({ "hookSpecificOutput": { "hookEventName": "PermissionRequest",
        "decision": { "behavior": "deny", "message": "Denied from Desk." } } })
}

/// No decision: Claude Code shows its own permission dialog in the terminal.
pub fn ask_in_terminal() -> Value {
    json!({})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_start_is_a_command_hook_and_the_rest_are_http() {
        let s = hook_settings("http://127.0.0.1:9/hook/a/b", 3600);
        let start = &s["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(start["type"], "command");
        assert!(start["command"]
            .as_str()
            .unwrap()
            .contains("'http://127.0.0.1:9/hook/a/b'"));
        for e in TOOL_EVENTS.iter().chain(PLAIN_EVENTS.iter()) {
            assert_eq!(s["hooks"][e][0]["hooks"][0]["type"], "http", "{e}");
        }
        assert_eq!(
            s["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"],
            3600
        );
        assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"], "*");
    }

    #[test]
    fn spawn_args_choose_or_resume_the_conversation() {
        let env = BTreeMap::new();
        let base = |launch| ClaudeSpawn {
            program: Path::new("/usr/local/bin/claude"),
            cwd: Path::new("/repo"),
            name: "auth-fix",
            launch,
            prompt: Some("continue"),
            hook_url: "http://h",
            approval_timeout_secs: 60,
            env: &env,
        };
        let new = spawn(&base(Launch::New { session_id: "abc" }));
        assert_eq!(&new.args[..4], ["--session-id", "abc", "-n", "auth-fix"]);
        assert_eq!(
            new.env.get(USAGE_URL).map(String::as_str),
            Some("http://h"),
            "Desk's plugin posts the plan's usage to the session's hooks"
        );
        assert_eq!(new.args.last().unwrap(), "continue");
        let resumed = spawn(&base(Launch::Resume { session_id: "abc" }));
        assert_eq!(&resumed.args[..2], ["--resume", "abc"]);
    }

    #[test]
    fn hook_payloads_parse_into_state_changes() {
        let start = br#"{"hook_event_name":"SessionStart","session_id":"s1","transcript_path":"/t.jsonl","source":"resume"}"#;
        assert_eq!(
            parse_hook(start).unwrap(),
            Hook::SessionStart {
                session_id: Some("s1".into()),
                transcript: Some("/t.jsonl".into()),
                resumed: true
            }
        );
        let perm = br#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"cargo test -p hub\nmore"}}"#;
        assert_eq!(
            parse_hook(perm).unwrap(),
            Hook::Permission {
                tool: "Bash(cargo test -p hub)".into(),
                detail: "cargo test -p hub".into()
            }
        );
        let stop = br#"{"hook_event_name":"Stop","transcript_path":"/t.jsonl","last_assistant_message":"desk-e2e-dir"}"#;
        assert_eq!(
            parse_hook(stop).unwrap(),
            Hook::Stop {
                transcript: Some("/t.jsonl".into()),
                reply: Some("desk-e2e-dir".into()),
                background: None
            }
        );
        let waiting =
            br#"{"hook_event_name":"Stop","last_assistant_message":"launched","background_tasks":[
            {"id":"a471","type":"subagent","status":"running","description":"Run sleep"},
            {"id":"baoj","type":"shell","status":"running","command":"sleep 30"},
            {"id":"done1","type":"shell","status":"completed"}],"session_crons":[]}"#;
        assert_eq!(
            parse_hook(waiting).unwrap(),
            Hook::Stop {
                transcript: None,
                reply: Some("launched".into()),
                background: Some(vec![
                    ("a471".into(), TaskKind::Agent),
                    ("baoj".into(), TaskKind::Shell)
                ])
            }
        );
        let woken = br#"{"hook_event_name":"UserPromptSubmit","prompt":"<task-notification>\n<task-id>baoj</task-id>\n<status>completed</status>\n</task-notification>"}"#;
        assert_eq!(
            parse_hook(woken).unwrap(),
            Hook::Woken,
            "not the person's prompt"
        );
        let inside = br#"{"hook_event_name":"PreToolUse","agent_id":"a471","agent_type":"general-purpose","tool_name":"Bash","tool_input":{"command":"sleep 20"}}"#;
        assert_eq!(agent_of(inside).as_deref(), Some("a471"));
        assert_eq!(
            parse_hook(br#"{"hook_event_name":"SubagentStop","agent_id":"a471"}"#).unwrap(),
            Hook::SubagentStop("a471".into())
        );
        let usage = br#"{"hook_event_name":"DeskUsage","rate_limits":[{"kind":"five_hour","percentUsed":62}]}"#;
        assert_eq!(
            parse_hook(usage).unwrap(),
            Hook::Usage(json!([{"kind": "five_hour", "percentUsed": 62}]))
        );
        let idle = br#"{"hook_event_name":"Notification","notification_type":"idle_prompt","message":"waiting"}"#;
        assert_eq!(parse_hook(idle).unwrap(), Hook::Other);
        let ask = br#"{"hook_event_name":"Notification","notification_type":"permission_prompt","message":"Claude needs your permission"}"#;
        assert_eq!(
            parse_hook(ask).unwrap(),
            Hook::NeedsInput("Claude needs your permission".into())
        );
    }

    #[test]
    fn mcp_tools_read_as_server_and_tool() {
        let (label, _) = tool_summary("mcp__workforce__memory_recall", None);
        assert_eq!(label, "workforce · memory_recall");
    }
}

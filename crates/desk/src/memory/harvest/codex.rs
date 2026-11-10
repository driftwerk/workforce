//! A Codex rollout (`~/.codex/sessions/…/rollout-….jsonl`) as parts.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{asker_of, one_line, time};
use crate::memory::record::Part;

/// What the harvester carries from line to line, kept with the record's
/// cursor: the turn, and how many of its items have been read, which keys
/// an exchange as `<turn>:<item>` (a forked rollout copies both).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexParts {
    #[serde(default)]
    pub turn: Option<String>,
    #[serde(default)]
    pub item: u32,
}

impl CodexParts {
    fn turn(&mut self, turn: Option<&str>) {
        if let Some(turn) = turn {
            if self.turn.as_deref() != Some(turn) {
                self.turn = Some(turn.to_string());
                self.item = 0;
            }
        }
    }
}

/// The parts one rollout line holds, with `state` carried on:
/// - `turn_context` and `task_started` name the turn;
/// - a `response_item` `message` is an Ask (the user's) or Text (the
///   assistant's); Codex's own context it sends as the user's
///   (`<environment_context>`, `<user_instructions>`, AGENTS.md) is left
///   out;
/// - `function_call`, `custom_tool_call` and `local_shell_call` are Calls,
///   and their `*_output`s Results;
/// - `task_complete` (and `turn_aborted`) are an End, and `compacted` the
///   summary Codex keeps at a compaction.
///
/// `reasoning` items are encrypted and never read.
pub fn codex_part(v: &Value, state: &mut CodexParts) -> Vec<Part> {
    let Some(p) = v.get("payload") else {
        return Vec::new();
    };
    let text = |k: &str| p.get(k).and_then(Value::as_str);
    let at = time(v, "/timestamp");
    match (
        v.get("type").and_then(Value::as_str),
        p.get("type").and_then(Value::as_str),
    ) {
        (Some("turn_context"), _) | (Some("event_msg"), Some("task_started" | "turn_started")) => {
            state.turn(text("turn_id"));
            Vec::new()
        }
        (Some("event_msg"), Some("task_complete" | "turn_complete" | "turn_aborted")) => {
            at.map(|at| Part::End { at }).into_iter().collect()
        }
        (Some("compacted"), _) => match (at, text("message")) {
            (Some(at), Some(message)) => vec![Part::Compacted {
                key: format!(
                    "{}:{}:compacted",
                    state.turn.as_deref().unwrap_or("-"),
                    state.item
                ),
                at,
                text: message.to_string(),
            }],
            _ => Vec::new(),
        },
        (Some("response_item"), Some(kind)) => {
            state.item += 1;
            item(kind, p, at, state)
        }
        _ => Vec::new(),
    }
}

fn item(
    kind: &str,
    p: &Value,
    at: Option<chrono::DateTime<chrono::Utc>>,
    state: &CodexParts,
) -> Vec<Part> {
    let id = || p["call_id"].as_str().unwrap_or_default().to_string();
    match kind {
        "message" => {
            let said: Vec<&str> = p["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| matches!(c["type"].as_str(), Some("input_text" | "output_text")))
                .filter_map(|c| c["text"].as_str())
                .collect();
            let said = said.join("\n");
            match p["role"].as_str() {
                Some("user") if !codex_context(&said) => match at {
                    Some(at) => vec![Part::Ask {
                        key: format!("{}:{}", state.turn.as_deref().unwrap_or("-"), state.item),
                        at,
                        asker: asker_of(&said),
                        text: said,
                    }],
                    None => Vec::new(),
                },
                Some("assistant") => vec![Part::Text(said)],
                _ => Vec::new(),
            }
        }
        "function_call" => {
            let tool = p["name"].as_str().unwrap_or("tool").to_string();
            let args = p["arguments"].as_str().unwrap_or_default();
            let parsed: Value = serde_json::from_str(args).unwrap_or(Value::Null);
            let command = matches!(tool.as_str(), "shell" | "exec_command" | "container.exec")
                .then(|| command_of(&parsed))
                .flatten();
            vec![Part::Call {
                id: id(),
                input: command.clone().unwrap_or_else(|| args.to_string()),
                tool,
                files: Vec::new(),
                command,
            }]
        }
        "custom_tool_call" => {
            let tool = p["name"].as_str().unwrap_or("tool").to_string();
            let input = p["input"].as_str().unwrap_or_default();
            let files = match tool.as_str() {
                "apply_patch" => patch_files(input),
                _ => Vec::new(),
            };
            vec![Part::Call {
                id: id(),
                tool,
                input: files.first().cloned().unwrap_or_else(|| input.to_string()),
                files,
                command: None,
            }]
        }
        "local_shell_call" => {
            let command = command_of(&p["action"]);
            vec![Part::Call {
                id: id(),
                tool: "shell".into(),
                input: command.clone().unwrap_or_default(),
                files: Vec::new(),
                command,
            }]
        }
        "function_call_output" | "custom_tool_call_output" => {
            let (text, failed) = output_of(&p["output"]);
            vec![Part::Result {
                id: id(),
                text,
                failed,
            }]
        }
        _ => Vec::new(),
    }
}

/// What Codex sends as the user's but the person didn't write: its context
/// blocks (`<environment_context>`, `<user_instructions>`,
/// `<recommended_plugins>` and the like, each one whole `<snake_case>`
/// block), and AGENTS.md.
fn codex_context(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t.starts_with("# AGENTS.md instructions") {
        return true;
    }
    let Some(rest) = t.strip_prefix('<') else {
        return false;
    };
    let Some((tag, _)) = rest.split_once('>') else {
        return false;
    };
    !tag.is_empty()
        && tag.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        && t.ends_with(&format!("</{tag}>"))
}

/// A shell call's command: `["bash", "-lc", "cargo test"]` as `cargo test`,
/// or a `cmd` string.
fn command_of(v: &Value) -> Option<String> {
    if let Some(cmd) = v["cmd"].as_str() {
        return Some(cmd.to_string());
    }
    let parts: Vec<&str> = v["command"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    match parts.as_slice() {
        [shell, flag, script]
            if flag.starts_with('-') && flag.ends_with('c') && shell.contains("sh") =>
        {
            Some(script.to_string())
        }
        _ => Some(parts.join(" ")),
    }
}

/// The files an `apply_patch` names: its `*** Add File:`, `*** Update
/// File:`, `*** Delete File:` and `*** Move to:` headers.
fn patch_files(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|l| {
            [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ]
            .iter()
            .find_map(|h| l.strip_prefix(h))
        })
        .map(|f| f.trim().to_string())
        .collect()
}

/// A call's output and whether it failed: a shell's JSON
/// (`{"output", "metadata": {"exit_code"}}`), its text form (`Exit code:
/// 1`), or an object with `success`.
fn output_of(v: &Value) -> (String, bool) {
    let from_json = |o: &Value| -> Option<(String, bool)> {
        let text = o.get("output").or_else(|| o.get("content")).map(one_line)?;
        let code = o.pointer("/metadata/exit_code").and_then(Value::as_i64);
        let failed = code.is_some_and(|c| c != 0) || o.get("success") == Some(&Value::Bool(false));
        Some((text, failed))
    };
    match v {
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(o @ Value::Object(_)) => from_json(&o).unwrap_or_else(|| (s.clone(), false)),
            _ => {
                let failed = s
                    .strip_prefix("Exit code: ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|c| c.parse::<i64>().ok())
                    .is_some_and(|c| c != 0);
                (s.clone(), failed)
            }
        },
        Value::Object(_) => from_json(v).unwrap_or_else(|| (v.to_string(), false)),
        other => (one_line(other), false),
    }
}

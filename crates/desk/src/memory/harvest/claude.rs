//! Claude Code's transcript (`~/.claude/projects/…/<id>.jsonl`) as parts.

use serde_json::Value;

use super::{asker_of, one_line, time};
use crate::memory::record::Part;

/// The parts one transcript line holds, keyed by the line's `uuid`:
/// - a `user` line's words are an Ask, and its `tool_result` blocks are
///   Results;
/// - an `assistant` line's `text` blocks are Text and its `tool_use`
///   blocks Calls (its `thinking` is never read), and a message that
///   stopped for good (any `stop_reason` but `tool_use` and `pause_turn`:
///   the turn's end, a stop sequence, its token limit, a refusal) is an End;
/// - the person stopping it (`[Request interrupted by user…]`) is an End,
///   never an Ask;
/// - the summary Claude Code writes at a compaction (`isCompactSummary`)
///   is Compacted.
///
/// A sub-agent's lines (`isSidechain`) are its parent's step: the parent's
/// call's result holds what it found. Claude Code's own messages
/// (`isMeta`, the tags it writes for slash and shell commands, `<synthetic>`
/// replies) are no one's words; any other prompt, `<` or not, is.
pub fn claude_part(v: &Value) -> Vec<Part> {
    if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return Vec::new();
    }
    let Some(at) = time(v, "/timestamp") else {
        return Vec::new();
    };
    let key = || {
        v.get("uuid")
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                let session = v.get("sessionId").and_then(Value::as_str).unwrap_or("");
                format!("{session}@{}", at.to_rfc3339())
            })
    };
    let content = v.pointer("/message/content");
    match v.get("type").and_then(Value::as_str) {
        Some("user") => {
            let text = match content {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(blocks)) => blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            if v.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
                return vec![Part::Compacted {
                    key: key(),
                    at,
                    text,
                }];
            }
            if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
                return Vec::new();
            }
            let mut parts: Vec<Part> = content
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "tool_result")
                .map(|b| Part::Result {
                    id: b["tool_use_id"].as_str().unwrap_or_default().to_string(),
                    text: result_text(&b["content"]),
                    failed: b["is_error"] == true,
                })
                .collect();
            if text.trim_start().starts_with(INTERRUPTED) {
                parts.push(Part::End { at });
                return parts;
            }
            let asker = asker_of(&text);
            let own = CLAUDE_TAGS.iter().any(|t| text.trim_start().starts_with(t));
            if !text.trim().is_empty() && !own {
                parts.insert(
                    0,
                    Part::Ask {
                        key: key(),
                        at,
                        asker,
                        text,
                    },
                );
            }
            parts
        }
        Some("assistant") => {
            if v.pointer("/message/model").and_then(Value::as_str) == Some("<synthetic>") {
                return Vec::new();
            }
            let mut parts: Vec<Part> = content
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|b| match b["type"].as_str() {
                    Some("text") => b["text"].as_str().map(|t| Part::Text(t.to_string())),
                    Some("tool_use") => Some(call(b)),
                    _ => None,
                })
                .collect();
            let stopped = v.pointer("/message/stop_reason").and_then(Value::as_str);
            if stopped.is_some_and(|r| !matches!(r, "tool_use" | "pause_turn")) {
                parts.push(Part::End { at });
            }
            parts
        }
        _ => Vec::new(),
    }
}

/// What Claude Code writes when the person stops a turn: the turn's end.
const INTERRUPTED: &str = "[Request interrupted by user";

/// The tags Claude Code wraps its own lines in: a slash command, its
/// output, and a `!` shell command and what it printed.
const CLAUDE_TAGS: [&str; 8] = [
    "<command-name>",
    "<command-message>",
    "<command-args>",
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<bash-input>",
    "<bash-stdout>",
    "<bash-stderr>",
];

/// A `tool_use` block as a call: its files from `file_path` (and a
/// notebook's path), its command from a shell's `command`.
fn call(b: &Value) -> Part {
    let tool = b["name"].as_str().unwrap_or("tool").to_string();
    let input = &b["input"];
    let files: Vec<String> = ["file_path", "notebook_path"]
        .iter()
        .filter_map(|k| input[*k].as_str())
        .map(str::to_string)
        .collect();
    let command = matches!(tool.as_str(), "Bash" | "BashOutput" | "PowerShell")
        .then(|| input["command"].as_str().map(str::to_string))
        .flatten();
    let summary = command
        .clone()
        .or_else(|| files.first().cloned())
        .unwrap_or_else(|| one_line(input));
    Part::Call {
        id: b["id"].as_str().unwrap_or_default().to_string(),
        tool,
        input: summary,
        files,
        command,
    }
}

fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|p| match p["type"].as_str() {
                Some("image") => "[image]".to_string(),
                _ => p["text"].as_str().unwrap_or_default().to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

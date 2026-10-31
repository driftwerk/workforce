//! Reads the end of a Claude Code transcript (`~/.claude/projects/…/<id>.jsonl`)
//! for what a session card shows when its terminal isn't attached: the last
//! prompt, the last reply, and Claude Code's own title for the conversation.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::asking::Denied;

const TAIL_BYTES: u64 = 512 * 1024;
const REPLY_MAX: usize = 600;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tail {
    pub title: Option<String>,
    pub last_prompt: Option<String>,
    pub last_reply: Option<String>,
    /// The last reply whole, for reading whether it asks the person
    /// something.
    pub said: Option<String>,
}

/// The whole lines of the end of a transcript.
fn tail_lines(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut raw = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut raw)?;
    let buf = String::from_utf8_lossy(&raw);
    // Starting mid-file means the first line is partial.
    Ok(buf
        .lines()
        .skip(usize::from(start > 0))
        .collect::<Vec<_>>()
        .join("\n"))
}

pub fn read_tail(path: &Path) -> std::io::Result<Tail> {
    Ok(tail_lines(path)?
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .fold(Tail::default(), |tail, v| absorb(tail, &v)))
}

/// What auto mode refused in the turn a Claude Code transcript ends on.
pub fn denials(path: &Path) -> std::io::Result<Vec<Denied>> {
    Ok(crate::asking::denials_in(tail_lines(path)?.lines()))
}

fn absorb(mut tail: Tail, v: &Value) -> Tail {
    let str_at = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    match v.get("type").and_then(Value::as_str) {
        Some("ai-title") => tail.title = str_at("aiTitle").or(tail.title),
        Some("last-prompt") => tail.last_prompt = str_at("lastPrompt").or(tail.last_prompt),
        Some("user") => {
            if let Some(text) = v.pointer("/message/content").and_then(Value::as_str) {
                if !text.starts_with('<') {
                    tail.last_prompt = Some(text.to_string());
                }
            }
        }
        Some("assistant") => {
            let text: Vec<&str> = v
                .pointer("/message/content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            if !text.is_empty() {
                let said = text.join("\n");
                tail.last_reply = Some(clip(&said));
                tail.said = Some(said);
            }
        }
        _ => {}
    }
    tail
}

/// The same for a Codex rollout: the last prompt the person typed and the
/// last thing Codex said when a turn completed.
pub fn read_codex_tail(path: &Path) -> std::io::Result<Tail> {
    Ok(tail_lines(path)?
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .fold(Tail::default(), |mut tail, v| {
            let payload = &v["payload"];
            match (v["type"].as_str(), payload["type"].as_str()) {
                (Some("event_msg"), Some("task_complete")) => {
                    if let Some(m) = payload["last_agent_message"]
                        .as_str()
                        .filter(|m| !m.trim().is_empty())
                    {
                        tail.last_reply = Some(clip(m));
                        tail.said = Some(m.to_string());
                    }
                }
                (Some("response_item"), Some("message")) if payload["role"] == "user" => {
                    let text = payload["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|c| c["text"].as_str())
                        .find(|t| !t.trim_start().starts_with('<'));
                    if let Some(t) = text {
                        tail.last_prompt = Some(t.to_string());
                    }
                }
                _ => {}
            }
            tail
        }))
}

pub fn clip(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= REPLY_MAX {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(REPLY_MAX).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn picks_the_latest_title_prompt_and_reply() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in [
            r#"{"type":"user","message":{"role":"user","content":"first ask"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Done, tests pass."}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#,
            r#"{"type":"ai-title","aiTitle":"Auth race fix"}"#,
            "not json",
            r#"{"type":"last-prompt","lastPrompt":"now open a PR"}"#,
        ] {
            writeln!(f, "{line}").unwrap();
        }
        assert_eq!(
            read_tail(f.path()).unwrap(),
            Tail {
                title: Some("Auth race fix".into()),
                last_prompt: Some("now open a PR".into()),
                last_reply: Some("Done, tests pass.".into()),
                said: Some("Done, tests pass.".into()),
            }
        );
    }

    #[test]
    fn codex_rollouts_give_the_last_prompt_and_answer() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in [
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x</environment_context>"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Run the tests"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"All 412 pass."}}"#,
        ] {
            writeln!(f, "{line}").unwrap();
        }
        let t = read_codex_tail(f.path()).unwrap();
        assert_eq!(
            (t.last_prompt.as_deref(), t.last_reply.as_deref()),
            (Some("Run the tests"), Some("All 412 pass."))
        );
    }

    #[test]
    fn a_missing_transcript_is_an_error() {
        assert!(read_tail(Path::new("/nonexistent/x.jsonl")).is_err());
    }
}

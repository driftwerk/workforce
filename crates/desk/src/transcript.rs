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
    /// How its last turn failed, when the transcript ends on Claude Code's
    /// API error: the plan's limit (which window, and when it resets), or
    /// another failure.
    pub failure: Option<crate::limit::TurnFailure>,
    /// The last `/login` run in it: its line's id and when. It signs the
    /// CLI in again, so the session runs on that sign-in from then on.
    pub last_login: Option<(String, chrono::DateTime<chrono::Utc>)>,
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

/// Which of the calls `ids` (`tool_use_id`s) the transcript's tail has a
/// result for: they ended, however the hooks that would have said so went.
pub fn results_among(
    path: &Path,
    ids: &std::collections::HashSet<String>,
) -> std::io::Result<std::collections::HashSet<String>> {
    Ok(results_in(tail_lines(path)?.lines(), ids))
}

fn results_in<'a>(
    lines: impl Iterator<Item = &'a str>,
    ids: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    lines
        .filter(|l| l.contains("tool_result"))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .flat_map(|v| {
            v.pointer("/message/content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|b| {
            b.get("tool_use_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|id| ids.contains(id))
        .collect()
}

fn absorb(mut tail: Tail, v: &Value) -> Tail {
    let str_at = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    match v.get("type").and_then(Value::as_str) {
        Some("ai-title") => tail.title = str_at("aiTitle").or(tail.title),
        Some("last-prompt") => tail.last_prompt = str_at("lastPrompt").or(tail.last_prompt),
        Some("user") => {
            if let Some(text) = v.pointer("/message/content").and_then(Value::as_str) {
                if text.starts_with("<command-name>/login</command-name>") {
                    let at = str_at("timestamp")
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
                        .map(|t| t.to_utc());
                    if let (Some(id), Some(at)) = (str_at("uuid"), at) {
                        tail.last_login = Some((id, at));
                    }
                }
                if !text.starts_with('<') {
                    tail.last_prompt = Some(text.to_string());
                    // Asked again: what stopped the last turn is behind it.
                    tail.failure = None;
                }
            }
        }
        Some("assistant") => {
            // Only the turn it ends on: a later reply or error clears it.
            tail.failure = crate::limit::in_transcript_line(v);
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
                failure: None,
                last_login: None,
            }
        );
    }

    #[test]
    fn a_login_run_inside_is_its_last() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in [
            r#"{"type":"user","uuid":"u1","timestamp":"2026-11-07T08:00:00.000Z","message":{"role":"user","content":"<command-name>/login</command-name>\n            <command-message>login</command-message>\n            <command-args></command-args>"}}"#,
            r#"{"type":"user","uuid":"u2","timestamp":"2026-11-07T08:05:00.000Z","message":{"role":"user","content":"fix it"}}"#,
            r#"{"type":"user","uuid":"u3","timestamp":"2026-11-07T09:00:00.000Z","message":{"role":"user","content":"<command-name>/login</command-name>"}}"#,
        ] {
            writeln!(f, "{line}").unwrap();
        }
        let t = read_tail(f.path()).unwrap();
        assert_eq!(
            t.last_login,
            Some((
                "u3".into(),
                chrono::DateTime::parse_from_rfc3339("2026-11-07T09:00:00Z")
                    .unwrap()
                    .to_utc()
            ))
        );
        assert_eq!(t.last_prompt.as_deref(), Some("fix it"));
    }

    #[test]
    fn a_transcript_that_ends_on_the_plans_limit_says_when_it_resets() {
        let refused = r#"{"type":"assistant","isApiErrorMessage":true,"apiError":"usage_limit_reached","apiErrorParams":{"rate_limit_info":{"status":"rejected","resetsAt":1792137600,"rateLimitType":"seven_day"}},"quotaLimits":{"status":"rejected","resetsAt":1792137600,"rateLimitType":"seven_day"},"error":"rate_limit","message":{"content":[{"type":"text","text":"You've hit your weekly limit · resets Fri 9am"}]}}"#;
        let write = |lines: &[&str]| {
            let mut f = tempfile::NamedTempFile::new().unwrap();
            for line in lines {
                writeln!(f, "{line}").unwrap();
            }
            f
        };
        let f = write(&[
            r#"{"type":"user","message":{"role":"user","content":"fix it"}}"#,
            refused,
        ]);
        let t = read_tail(f.path()).unwrap();
        assert_eq!(
            t.failure,
            Some(crate::limit::TurnFailure::Limit(crate::limit::LimitHit {
                window: Some("seven_day".into()),
                resets_at: chrono::DateTime::from_timestamp(1_792_137_600, 0),
            }))
        );
        assert_eq!(
            t.last_reply.as_deref(),
            Some("You've hit your weekly limit · resets Fri 9am")
        );
        // Asked again, or answered since: no longer on it.
        let f = write(&[
            refused,
            r#"{"type":"user","message":{"role":"user","content":"Continue"}}"#,
        ]);
        assert_eq!(read_tail(f.path()).unwrap().failure, None);
        let f = write(&[
            refused,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Done."}]}}"#,
        ]);
        assert_eq!(read_tail(f.path()).unwrap().failure, None);
        // A 429 that isn't the plan's: a failed turn.
        let f = write(&[
            r#"{"type":"user","message":{"role":"user","content":"fix it"}}"#,
            r#"{"type":"assistant","isApiErrorMessage":true,"apiError":null,"quotaLimits":null,"error":"rate_limit","apiErrorStatus":429,"message":{"content":[{"type":"text","text":"API Error: Request rejected (429) · This request would exceed your account's rate limit."}]}}"#,
        ]);
        assert_eq!(
            read_tail(f.path()).unwrap().failure,
            Some(crate::limit::TurnFailure::Failed("rate_limit".into()))
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

    #[test]
    fn a_calls_result_in_the_transcript_says_it_ended() {
        let lines = [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_A","name":"Bash","input":{}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_A","content":"ok"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_B","name":"Bash","input":{}}]}}"#,
        ];
        let ids = ["toolu_A", "toolu_B"]
            .map(String::from)
            .into_iter()
            .collect();
        assert_eq!(
            results_in(lines.into_iter(), &ids),
            ["toolu_A".to_string()].into_iter().collect()
        );
    }
}

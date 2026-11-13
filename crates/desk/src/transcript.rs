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
    /// When Claude Code last wrote a reply that wasn't an API error: a call
    /// on its sign-in that succeeded then.
    pub ok_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The last `/login` run in it that Claude Code said succeeded ("Login
    /// successful"): its line's id and when. It signs the CLI in again, so
    /// the session runs on that sign-in from then on. One cancelled or
    /// failed is not.
    pub last_login: Option<(String, chrono::DateTime<chrono::Utc>)>,
    /// A `/login` whose outcome is still to come.
    pending_login: Option<(String, chrono::DateTime<chrono::Utc>)>,
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
                    tail.pending_login = str_at("uuid").zip(at);
                } else if text.starts_with("<local-command-stdout>") {
                    // What the command said: only a success signs it in.
                    let pending = tail.pending_login.take();
                    if text.starts_with("<local-command-stdout>Login successful") {
                        tail.last_login = pending.or(tail.last_login.take());
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
                // A reply, not an API error: the call it came from succeeded.
                if tail.failure.is_none() && !crate::limit::says_plan_limit(&said) {
                    let at = str_at("timestamp")
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
                        .map(|t| t.to_utc());
                    tail.ok_at = at.or(tail.ok_at);
                }
                tail.last_reply = Some(clip(&said));
                tail.said = Some(said);
            }
        }
        _ => {}
    }
    tail
}

/// The `/login`s Claude Code confirmed in a session since its process
/// started, and the plan limit its turn stopped on last before the first:
/// on the credential it ran on until then.
#[derive(Debug, Clone, PartialEq)]
pub struct SignedInAgain {
    /// The first one since: from then on it ran on another sign-in.
    pub first_at: chrono::DateTime<chrono::Utc>,
    /// The last one's line id and when: the one a later read of the tail
    /// sees, so it is never taken for a new one.
    pub last_id: String,
    pub last_at: chrono::DateTime<chrono::Utc>,
    /// When its turn last stopped on the plan's limit before the first
    /// (since the process started), and the limit it stopped on.
    pub hit_before: Option<(chrono::DateTime<chrono::Utc>, crate::limit::LimitHit)>,
}

/// One transcript line, as far as a `/login` scan reads it: borrowed where
/// it can be, its content only when it is text.
#[derive(serde::Deserialize)]
struct LoginLine<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    uuid: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    timestamp: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    message: Option<LoginMessage<'a>>,
}

#[derive(serde::Deserialize)]
struct LoginMessage<'a> {
    #[serde(borrow)]
    content: Option<LoginContent<'a>>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum LoginContent<'a> {
    Text(#[serde(borrow)] std::borrow::Cow<'a, str>),
    Other(serde::de::IgnoredAny),
}

/// The `/login`s Claude Code confirmed in the whole transcript at or after
/// `since` (when its process started), however far back they are: a long
/// turn since buries them far past the tail. Only the lines that could say
/// one are read, and only those that do are parsed whole.
pub fn last_login_since(
    path: &Path,
    since: chrono::DateTime<chrono::Utc>,
) -> std::io::Result<Option<SignedInAgain>> {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut raw = Vec::new();
    let mut pending: Option<(String, chrono::DateTime<chrono::Utc>)> = None;
    let mut hit: Option<(chrono::DateTime<chrono::Utc>, crate::limit::LimitHit)> = None;
    let mut found: Option<SignedInAgain> = None;
    loop {
        raw.clear();
        if reader.read_until(b'\n', &mut raw)? == 0 {
            return Ok(found);
        }
        let line = String::from_utf8_lossy(&raw);
        let error = line.contains("\"isApiErrorMessage\"");
        if !(error
            || line.contains("/login</command-name>")
            || line.contains("<local-command-stdout>"))
        {
            continue;
        }
        let Ok(l) = serde_json::from_str::<LoginLine<'_>>(&line) else {
            continue;
        };
        // A line with no time can't be placed against the process's start:
        // never one inside it (Claude Code stamps every line it writes).
        let Some(at) = l
            .timestamp
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.to_utc())
        else {
            continue;
        };
        match l.kind.as_deref() {
            Some("user") => {
                let Some(LoginContent::Text(text)) = l.message.and_then(|m| m.content) else {
                    continue;
                };
                if text.starts_with("<command-name>/login</command-name>") {
                    pending = l.uuid.map(|id| (id.into_owned(), at));
                } else if text.starts_with("<local-command-stdout>") {
                    let Some((id, at)) = pending.take() else {
                        continue;
                    };
                    if !text.starts_with("<local-command-stdout>Login successful") {
                        continue;
                    }
                    if at < since {
                        // Before its process started: a limit before it
                        // was another sign-in's.
                        hit = None;
                        continue;
                    }
                    match &mut found {
                        // The first: what it stopped on before is the
                        // credential's it ran on until then.
                        None => {
                            found = Some(SignedInAgain {
                                first_at: at,
                                last_id: id,
                                last_at: at,
                                hit_before: hit.take(),
                            })
                        }
                        Some(f) => {
                            f.last_id = id;
                            f.last_at = at;
                        }
                    }
                }
            }
            Some("assistant") if error && at >= since && found.is_none() => {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if let Some(crate::limit::TurnFailure::Limit(h)) =
                        crate::limit::in_transcript_line(&v)
                    {
                        hit = Some((at, h));
                    }
                }
            }
            _ => {}
        }
    }
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
                (Some("event_msg"), Some("task_complete" | "turn_complete")) => {
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

/// Whether the engine did anything in the transcript at `path`, beyond
/// the prompt it was started on: a Claude Code assistant line, or a Codex
/// item that isn't the person's, its developer's or the system's message.
/// One too long to read whole did.
pub fn engine_acted(path: &Path, codex: bool) -> std::io::Result<bool> {
    if std::fs::metadata(path)?.len() > TAIL_BYTES {
        return Ok(true);
    }
    Ok(tail_lines(path)?
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .any(|v| match codex {
            false => v["type"] == "assistant",
            true => {
                let p = &v["payload"];
                v["type"] == "response_item"
                    && !(p["type"] == "message"
                        && matches!(p["role"].as_str(), Some("user" | "developer" | "system")))
            }
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
                ok_at: None,
                last_login: None,
                pending_login: None,
            }
        );
    }

    #[test]
    fn a_login_run_inside_is_its_last() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in [
            r#"{"type":"user","uuid":"u1","timestamp":"2026-11-07T08:00:00.000Z","message":{"role":"user","content":"<command-name>/login</command-name>\n            <command-message>login</command-message>\n            <command-args></command-args>"}}"#,
            r#"{"type":"user","uuid":"u2","timestamp":"2026-11-07T08:05:00.000Z","message":{"role":"user","content":"fix it"}}"#,
            r#"{"type":"user","uuid":"u1s","timestamp":"2026-11-07T08:00:05.000Z","message":{"role":"user","content":"<local-command-stdout>Login successful</local-command-stdout>"}}"#,
            r#"{"type":"user","uuid":"u3","timestamp":"2026-11-07T09:00:00.000Z","message":{"role":"user","content":"<command-name>/login</command-name>"}}"#,
            r#"{"type":"user","uuid":"u3s","timestamp":"2026-11-07T09:00:09.000Z","message":{"role":"user","content":"<local-command-stdout>Login successful</local-command-stdout>"}}"#,
            // Cancelled: not a sign-in.
            r#"{"type":"user","uuid":"u4","timestamp":"2026-11-07T10:00:00.000Z","message":{"role":"user","content":"<command-name>/login</command-name>"}}"#,
            r#"{"type":"user","uuid":"u4s","timestamp":"2026-11-07T10:00:03.000Z","message":{"role":"user","content":"<local-command-stdout>Login interrupted</local-command-stdout>"}}"#,
            // Still under way: not yet.
            r#"{"type":"user","uuid":"u5","timestamp":"2026-11-07T11:00:00.000Z","message":{"role":"user","content":"<command-name>/login</command-name>"}}"#,
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
    fn a_login_buried_past_the_tail_is_found_since_the_process_started_with_the_limit_before_it() {
        let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().to_utc();
        let login = |id: &str, t: &str| {
            [
                format!(
                    r#"{{"type":"user","uuid":"{id}","timestamp":"{t}","message":{{"role":"user","content":"<command-name>/login</command-name>\n<command-message>login</command-message>"}}}}"#
                ),
                format!(
                    r#"{{"type":"user","uuid":"{id}s","timestamp":"{t}","message":{{"role":"user","content":"<local-command-stdout>Login successful</local-command-stdout>"}}}}"#
                ),
            ]
        };
        let full = |t: &str| {
            format!(
                r#"{{"type":"assistant","timestamp":"{t}","isApiErrorMessage":true,"apiError":"usage_limit_reached","apiErrorParams":{{"rate_limit_info":{{"status":"rejected","resetsAt":1794466800,"rateLimitType":"seven_day"}}}},"error":"rate_limit","message":{{"content":[{{"type":"text","text":"You've hit your weekly limit"}}]}}}}"#
            )
        };
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let mut lines = vec![full("2026-11-05T07:32:30Z")];
        lines.extend(login("early", "2026-11-05T08:46:05Z"));
        lines.push(full("2026-11-07T07:52:29Z"));
        lines.extend(login("late", "2026-11-07T07:53:27Z"));
        // A turn's worth of tool output since: the login is long past the tail.
        let filler = format!(
            r#"{{"type":"user","timestamp":"2026-11-07T08:00:00Z","message":{{"role":"user","content":[{{"type":"tool_result","content":"{}"}}]}}}}"#,
            "x".repeat(4096)
        );
        lines.extend(std::iter::repeat_n(filler, 200));
        for line in &lines {
            writeln!(f, "{line}").unwrap();
        }
        assert!(
            read_tail(f.path()).unwrap().last_login.is_none(),
            "the tail alone never sees it"
        );
        // The process started on 11-05 at 09:28: the later login is inside
        // it, with the limit it stopped on just before.
        let found = last_login_since(f.path(), at("2026-11-05T09:28:42Z"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (found.first_at, found.last_id.as_str(), found.last_at),
            (
                at("2026-11-07T07:53:27Z"),
                "late",
                at("2026-11-07T07:53:27Z")
            )
        );
        let (hit_at, hit) = found.hit_before.unwrap();
        assert_eq!(hit_at, at("2026-11-07T07:52:29Z"));
        assert_eq!(
            (hit.window.as_deref(), hit.resets_at),
            (Some("seven_day"), Some(at("2026-11-12T07:00:00Z")))
        );
        // Started after it: no login inside this process.
        assert_eq!(
            last_login_since(f.path(), at("2026-11-07T08:00:00Z")).unwrap(),
            None
        );
        // Started before both: from the first, with the limit before it.
        let found = last_login_since(f.path(), at("2026-11-01T00:00:00Z"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (found.first_at, found.last_id.as_str()),
            (at("2026-11-05T08:46:05Z"), "late")
        );
        assert_eq!(found.hit_before.unwrap().0, at("2026-11-05T07:32:30Z"));
    }

    #[test]
    fn two_logins_inside_a_process_take_it_off_its_account_at_the_first() {
        let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().to_utc();
        let lines = [
            r#"{"type":"assistant","timestamp":"2026-11-07T07:00:00Z","isApiErrorMessage":true,"apiError":"usage_limit_reached","apiErrorParams":{"rate_limit_info":{"status":"rejected","resetsAt":1794466800,"rateLimitType":"seven_day"}},"message":{"content":[]}}"#,
            r#"{"type":"user","uuid":"one","timestamp":"2026-11-07T07:10:00Z","message":{"role":"user","content":"<command-name>/login</command-name>"}}"#,
            r#"{"type":"user","uuid":"one-s","timestamp":"2026-11-07T07:10:00Z","message":{"role":"user","content":"<local-command-stdout>Login successful</local-command-stdout>"}}"#,
            // Its new sign-in's own limit: never the account's.
            r#"{"type":"assistant","timestamp":"2026-11-07T08:00:00Z","isApiErrorMessage":true,"apiError":"usage_limit_reached","apiErrorParams":{"rate_limit_info":{"status":"rejected","resetsAt":1794596400,"rateLimitType":"seven_day"}},"message":{"content":[]}}"#,
            r#"{"type":"user","uuid":"two","timestamp":"2026-11-07T08:10:00Z","message":{"role":"user","content":"<command-name>/login</command-name>"}}"#,
            r#"{"type":"user","uuid":"two-s","timestamp":"2026-11-07T08:10:00Z","message":{"role":"user","content":"<local-command-stdout>Login successful</local-command-stdout>"}}"#,
            // No time: never placed against the start.
            r#"{"type":"user","uuid":"three","message":{"role":"user","content":"<command-name>/login</command-name>"}}"#,
        ];
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        let found = last_login_since(f.path(), at("2026-11-07T06:00:00Z"))
            .unwrap()
            .unwrap();
        assert_eq!(found.first_at, at("2026-11-07T07:10:00Z"));
        assert_eq!(
            (found.last_id.as_str(), found.last_at),
            ("two", at("2026-11-07T08:10:00Z"))
        );
        let (hit_at, hit) = found.hit_before.unwrap();
        assert_eq!(
            (hit_at, hit.resets_at),
            (at("2026-11-07T07:00:00Z"), Some(at("2026-11-12T07:00:00Z")))
        );
    }

    #[test]
    fn a_transcript_says_when_its_last_call_succeeded_and_when_a_refusal_was_written() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in [
            r#"{"type":"user","timestamp":"2026-11-13T08:00:00Z","message":{"role":"user","content":"fix it"}}"#,
            r#"{"type":"assistant","timestamp":"2026-11-13T08:01:00Z","message":{"content":[{"type":"text","text":"Fixed."}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-11-13T08:15:59Z","isApiErrorMessage":true,"apiError":"usage_limit_reached","quotaLimits":{"status":"rejected","resetsAt":1795071600,"rateLimitType":"seven_day"},"error":"rate_limit","message":{"content":[{"type":"text","text":"You've hit your weekly limit · resets Nov 19, 7am (UTC)"}]}}"#,
        ] {
            writeln!(f, "{line}").unwrap();
        }
        let at = |t: &str| chrono::DateTime::parse_from_rfc3339(t).unwrap().to_utc();
        let t = read_tail(f.path()).unwrap();
        assert_eq!(t.ok_at, Some(at("2026-11-13T08:01:00Z")), "not the refusal");
        let hit = t.failure.unwrap().limit().cloned().unwrap();
        assert_eq!(hit.at, Some(at("2026-11-13T08:15:59Z")));
        // Answered since: the last call that succeeded is that one.
        writeln!(
            f,
            r#"{{"type":"assistant","timestamp":"2026-11-13T11:30:00Z","message":{{"content":[{{"type":"text","text":"Done."}}]}}}}"#
        )
        .unwrap();
        let t = read_tail(f.path()).unwrap();
        assert_eq!(t.ok_at, Some(at("2026-11-13T11:30:00Z")));
        assert_eq!(t.failure, None);
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
                at: None,
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

    #[test]
    fn an_engine_acted_once_it_said_or_did_anything_beyond_its_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, lines: &[&str]| {
            let p = dir.path().join(name);
            std::fs::write(&p, lines.join("\n")).unwrap();
            p
        };
        let prompt = r#"{"type":"user","message":{"content":"Call Desk's brief tool first"}}"#;
        let said =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reading it."}]}}"#;
        assert!(!engine_acted(&file("c0", &[prompt]), false).unwrap());
        assert!(engine_acted(&file("c1", &[prompt, said]), false).unwrap());
        let asked =
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[]}}"#;
        let dev = r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[]}}"#;
        let call = r#"{"type":"response_item","payload":{"type":"function_call","name":"brief"}}"#;
        assert!(!engine_acted(&file("x0", &[dev, asked]), true).unwrap());
        assert!(engine_acted(&file("x1", &[dev, asked, call]), true).unwrap());
        assert!(engine_acted(&dir.path().join("none"), false).is_err());
    }
}

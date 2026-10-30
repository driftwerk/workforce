//! Whether a turn that ended is waiting on the person: a tool auto mode
//! refused, or a reply that asks them to decide, answer, approve or do
//! something. Rules read it first; a model reads only what they leave open,
//! and whatever it can't settle in time is done, never a false need.
//!
//! Verified on Claude Code 2.1.314 in auto mode: a refusal fires the
//! `PermissionDenied` hook (`tool_name`, `tool_input`, `tool_use_id`,
//! `reason`, e.g. `[Credential Exploration]`), and the transcript keeps it
//! as the tool's result: a `user` entry with `toolDenialKind`
//! (`automode-blocked`, `automode-unavailable`, `permission-rule`,
//! `user-rejected`) whose `tool_result` says "Permission for this action was
//! denied by the Claude Code auto mode classifier. Reason: [...]". A
//! `PreToolUse` hook answering `allow` runs the same call without the
//! classifier (the transcript records it as `source: hook`).

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

use crate::claude::tool_summary;
use crate::crew;

/// One tool call as an allow matches it: the tool, and, for one whose input
/// says what it does (a command, a file, a URL), that input exactly. Other
/// tools match whole.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Call {
    pub tool: String,
    pub input: Option<String>,
}

impl Call {
    pub fn of(tool: &str, input: Option<&Value>) -> Self {
        let field = |k: &str| {
            input
                .and_then(|i| i.get(k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let input = match tool {
            "Bash" | "PowerShell" | "Monitor" => field("command"),
            "Edit" | "Write" | "MultiEdit" | "Read" => field("file_path"),
            "NotebookEdit" => field("notebook_path"),
            "WebFetch" => field("url"),
            _ => None,
        };
        Self {
            tool: tool.to_string(),
            input,
        }
    }
}

/// A tool auto mode refused in the turn.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Denied {
    /// Its label: `Bash(security find-generic-password …)`.
    pub tool: String,
    pub detail: String,
    /// Why: the classifier's rule, `[Credential Exploration]`.
    pub reason: String,
    /// A classifier's verdict, which an allow from Desk gets past. A
    /// no-verdict refusal or a safety check isn't: only the person, in the
    /// terminal, can.
    pub allowable: bool,
    /// What allowing it allows.
    #[serde(skip)]
    pub call: Call,
}

impl Denied {
    pub fn new(tool: &str, input: Option<&Value>, reason: &str, allowable: bool) -> Self {
        let (label, detail) = tool_summary(tool, input);
        Self {
            tool: label,
            detail,
            reason: crew::clip(reason, 160),
            allowable,
            call: Call::of(tool, input),
        }
    }

    /// From a `PermissionDenied` hook: allowable when the classifier gave a
    /// verdict.
    pub fn from_hook(tool: &str, input: Option<&Value>, reason: &str) -> Self {
        Self::new(tool, input, reason, is_verdict(reason))
    }

    /// The tool's name: `Bash`, `workforce · memory_recall`.
    pub fn name(&self) -> &str {
        self.tool.split('(').next().unwrap_or(&self.tool).trim()
    }
}

/// Whether a refusal's reason is the classifier's verdict: not one it gave
/// none for ("Auto mode could not evaluate this action…", "Classifier
/// unavailable"), nor a safety check outside auto mode.
fn is_verdict(reason: &str) -> bool {
    let r = reason.trim();
    !r.is_empty()
        && !r.starts_with("Auto mode could not evaluate")
        && r != "Classifier unavailable"
        && !r.contains("safety check")
}

/// Why a session's turn waits on the person, as Desk shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Asked {
    /// What it needs: "Stopped: Bash was not allowed", or its question.
    pub ask: String,
    /// Why it was read so: "asked you to choose between 2 options".
    pub why: String,
    pub options: Option<Vec<String>>,
    /// The refusal it stopped on, when it did.
    pub denied: Option<Denied>,
}

impl Asked {
    /// The turn stopped on what auto mode refused: the last of it, which
    /// is what it gave up on.
    pub fn refused(denials: &[Denied]) -> Option<Self> {
        let last = denials.last()?;
        let more = match denials.len() - 1 {
            0 => String::new(),
            n => format!(" and {n} more"),
        };
        Some(Self {
            ask: format!("Stopped: {} was not allowed", last.name()),
            why: format!("auto mode refused {} ({}){more}", last.name(), last.reason),
            options: None,
            denied: Some(last.clone()),
        })
    }

    fn asks(ask: &str, why: impl Into<String>, options: Option<Vec<String>>) -> Self {
        Self {
            ask: crew::clip(ask, 240),
            why: why.into(),
            options,
            denied: None,
        }
    }

    /// A model read the reply as asking the person something.
    pub fn read_by_model(reply: &str) -> Self {
        let ask = crew::question_in(reply)
            .unwrap_or_else(|| last_sentence(&prose(reply)).unwrap_or_default());
        Self::asks(&ask, "asked you something", None)
    }
}

/// What the rules make of a reply.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    Asks(Asked),
    Done,
    /// It may ask, or not: a model's to read.
    Unsure,
}

/// The tools auto mode refused in the turn `lines` (a transcript's JSONL,
/// oldest first) ends on: after the person's last prompt, in the session's
/// own thread (not a sub-agent's).
pub fn denials_in<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<Denied> {
    let mut calls: HashMap<String, (String, Value)> = HashMap::new();
    let mut out = Vec::new();
    for v in lines.filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let blocks = v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                for b in blocks.iter().filter(|b| b["type"] == "tool_use") {
                    if let (Some(id), Some(name)) = (b["id"].as_str(), b["name"].as_str()) {
                        calls.insert(id.into(), (name.into(), b["input"].clone()));
                    }
                }
            }
            Some("user") if is_prompt(&v) => out.clear(),
            Some("user") => {
                let kind = v.get("toolDenialKind").and_then(Value::as_str);
                for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
                    let text = result_text(&b["content"]);
                    let kind =
                        kind.or_else(|| text.starts_with(CLASSIFIER).then_some("automode-blocked"));
                    let (
                        Some(
                            kind
                            @ ("automode-blocked" | "automode-unavailable" | "permission-rule"),
                        ),
                        Some(id),
                    ) = (kind, b["tool_use_id"].as_str())
                    else {
                        continue;
                    };
                    let (name, input) = calls
                        .get(id)
                        .map(|(n, i)| (n.as_str(), Some(i)))
                        .unwrap_or(("a tool", None));
                    out.push(Denied::new(
                        name,
                        input,
                        &reason_in(&text, kind),
                        kind == "automode-blocked",
                    ));
                }
            }
            _ => {}
        }
    }
    out
}

const CLASSIFIER: &str =
    "Permission for this action was denied by the Claude Code auto mode classifier";

/// The person's own prompt: words, not a tool's result or Claude Code's
/// own `<…>` messages.
fn is_prompt(v: &Value) -> bool {
    if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    match v.pointer("/message/content") {
        Some(Value::String(s)) => !s.trim_start().starts_with('<'),
        Some(Value::Array(blocks)) => {
            blocks.iter().all(|b| b["type"] != "tool_result")
                && blocks.iter().any(|b| {
                    b["type"] == "text"
                        && b["text"]
                            .as_str()
                            .is_some_and(|t| !t.trim_start().starts_with('<'))
                })
        }
        _ => false,
    }
}

fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// "… classifier. Reason: [Production Reads]. If you have …" → the reason.
fn reason_in(text: &str, kind: &str) -> String {
    if let Some((_, rest)) = text.split_once("Reason: ") {
        let end = rest
            .find(". If you")
            .or_else(|| rest.find(". "))
            .unwrap_or(rest.len());
        return rest[..end].trim().to_string();
    }
    match kind {
        "permission-rule" => "a safety check outside auto mode".into(),
        _ => "auto mode gave no verdict".into(),
    }
}

/// The reply without its code blocks.
fn prose(reply: &str) -> String {
    let mut out = String::new();
    let mut fenced = false;
    for line in reply.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced {
            out.push_str(line.trim());
            out.push('\n');
        }
    }
    out
}

fn last_sentence(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    let start = line[..line.len().saturating_sub(1)]
        .char_indices()
        .filter(|&(i, c)| {
            matches!(c, '.' | '!' | '?')
                && line[i + c.len_utf8()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace)
        })
        .map(|(i, c)| i + c.len_utf8())
        .next_back()
        .unwrap_or(0);
    Some(line[start..].trim().to_string()).filter(|s| !s.is_empty())
}

/// A list item's text: `- a`, `* a`, `1. a`, `2) a`.
fn item(line: &str) -> Option<&str> {
    line.strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| {
            let (n, rest) = line.split_once(". ").or_else(|| line.split_once(") "))?;
            (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).then_some(rest)
        })
}

/// The list a reply ends on (2 to 6 items) and the line that leads into it.
fn trailing_list(prose: &str) -> Option<(String, Vec<String>)> {
    let lines: Vec<&str> = prose.lines().filter(|l| !l.is_empty()).collect();
    let mut items = Vec::new();
    let mut at = lines.len();
    while at > 0 {
        match item(lines[at - 1]) {
            Some(i) => items.push(i),
            None => break,
        }
        at -= 1;
    }
    if !(2..=6).contains(&items.len()) || at == 0 {
        return None;
    }
    items.reverse();
    let options = items.iter().map(|i| option_of(i)).collect();
    Some((lines[at - 1].to_string(), options))
}

/// An option's name: its bold or code lead, else the item, short.
fn option_of(item: &str) -> String {
    let lead = item
        .strip_prefix("**")
        .and_then(|r| r.split_once("**"))
        .map(|(l, _)| l)
        .or_else(|| {
            item.strip_prefix('`')
                .and_then(|r| r.split_once('`'))
                .map(|(l, _)| l)
        })
        .unwrap_or(item);
    crew::clip(lead.trim().trim_end_matches(':').trim(), 80)
}

/// Words that put a choice to the person.
const CHOOSE: &[&str] = &[
    "either", "which", "options", "choices", "ways", "prefer", "choose", "pick", "decide", "or",
];

/// Whether `text` holds `phrase` as whole words, any case.
fn says(text: &str, phrase: &str) -> bool {
    let words = |s: &str| -> String {
        let cleaned: String = s
            .to_lowercase()
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '\'' || c == '’' {
                    c
                } else {
                    ' '
                }
            })
            .collect();
        format!(
            " {} ",
            cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
        )
        .replace('’', "'")
    };
    words(text).contains(&words(phrase))
}

/// Questions that expect no answer: "…, right?", "Who knew?".
const RHETORICAL: &[&str] = &[
    "right?",
    "eh?",
    "huh?",
    "no?",
    "isn't it?",
    "wasn't it?",
    "aren't they?",
    "don't you think?",
    "who knew?",
    "what could go wrong?",
    "why not?",
    "go figure?",
    "how about that?",
];

/// Words that put a question to the person, or ask their leave.
const TO_YOU: &[&str] = &[
    "you",
    "your",
    "should i",
    "shall i",
    "may i",
    "can i",
    "should we",
    "shall we",
    "want me",
    "ok to",
    "okay to",
    "ok if",
    "okay if",
    "go ahead",
    "proceed",
    "which",
    "prefer",
];

/// What asks the person to approve, allow or do something, and how.
const ASKS: &[(&str, &str)] = &[
    ("your approval", "asked for your approval"),
    ("please approve", "asked for your approval"),
    ("approve it", "asked for your approval"),
    ("your permission", "asked for your permission"),
    ("permission rule", "asked for your permission"),
    ("allow it", "asked for your permission"),
    ("please confirm", "asked you to confirm"),
    ("can you confirm", "asked you to confirm"),
    ("need you to", "asked you to do something"),
    ("you'll need to", "asked you to do something"),
    ("you will need to", "asked you to do something"),
    ("please run", "asked you to do something"),
    ("run it yourself", "asked you to do something"),
    ("do it yourself", "asked you to do something"),
    ("yourself", "asked you to do something"),
    ("up to you", "asked you to decide"),
    ("your call", "asked you to decide"),
    ("tell me which", "asked you to decide"),
    ("let me know which", "asked you to decide"),
    ("waiting for you", "is waiting for you"),
    ("waiting on you", "is waiting for you"),
];

/// Offers that may or may not wait on the person.
const OFFERS: &[&str] = &[
    "let me know",
    "if you want me to",
    "if you'd like me to",
    "if you would like me to",
    "want me to",
    "should i",
];

/// What the rules make of a turn's last reply.
pub fn read(reply: &str) -> Reading {
    let prose = prose(reply);
    let paragraphs: Vec<&str> = prose
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let Some(last) = paragraphs.last() else {
        return Reading::Done;
    };
    if let Some((lead, options)) = trailing_list(&prose)
        .filter(|(lead, _)| lead.ends_with([':', '?']) && CHOOSE.iter().any(|w| says(lead, w)))
    {
        let ask = last_sentence(&lead).unwrap_or(lead);
        let why = format!("asked you to choose between {} options", options.len());
        return Reading::Asks(Asked::asks(ask.trim_end_matches(':'), why, Some(options)));
    }
    if let Some(q) = crew::question_in(reply) {
        let lower = q.to_lowercase();
        if RHETORICAL.iter().any(|r| lower.ends_with(r)) {
            return Reading::Done;
        }
        if let Some(options) = crew::options_in(reply, &q) {
            let why = format!("asked you to choose between {} options", options.len());
            return Reading::Asks(Asked::asks(&q, why, Some(options)));
        }
        // "Ship it?", "Proceed?": short, and only ever put to the person.
        if q.split_whitespace().count() <= 3 || TO_YOU.iter().any(|w| says(&q, w)) {
            return Reading::Asks(Asked::asks(&q, "asked you a question", None));
        }
        return Reading::Unsure;
    }
    // The paragraph it ends on, and the one leading into a list it ends on.
    let tail = match paragraphs.len() {
        n if n >= 2 && last.lines().all(|l| item(l).is_some()) => {
            format!("{}\n{last}", paragraphs[n - 2])
        }
        _ => last.to_string(),
    };
    let sentence_with = |phrase: &str| {
        tail.split_inclusive(['.', '!', '\n'])
            .find(|s| says(s, phrase))
            .map(|s| s.trim().trim_start_matches(['-', '*', ' ']).to_string())
    };
    if let Some((phrase, why)) = ASKS.iter().find(|(p, _)| says(&tail, p)) {
        let ask = sentence_with(phrase).unwrap_or_else(|| tail.clone());
        return Reading::Asks(Asked::asks(&ask, *why, None));
    }
    if OFFERS.iter().any(|p| says(&tail, p)) {
        return Reading::Unsure;
    }
    Reading::Done
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A turn as Claude Code 2.1.314 writes it, cut to what is read.
    fn turn(entries: &[Value]) -> String {
        entries
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn prompt(text: &str) -> Value {
        json!({ "type": "user", "message": { "role": "user", "content": text } })
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Value {
        json!({ "type": "assistant", "message": { "role": "assistant", "content": [
            { "type": "tool_use", "id": id, "name": name, "input": input }
        ] } })
    }

    fn result(id: &str, text: &str, kind: Option<&str>) -> Value {
        let mut v = json!({ "type": "user", "message": { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": id, "is_error": true, "content": text }
        ] } });
        if let Some(kind) = kind {
            v["toolDenialKind"] = json!(kind);
        }
        v
    }

    fn reply(text: &str) -> Value {
        json!({ "type": "assistant", "message": { "role": "assistant", "content": [
            { "type": "text", "text": text }
        ] } })
    }

    const REFUSED: &str = "Permission for this action was denied by the Claude Code auto mode classifier. Reason: [Credential Exploration]. If you have other tasks that don't depend on this action, continue working on those.";

    #[test]
    fn a_classifier_refusal_in_the_turn_is_read_with_its_tool_and_reason() {
        let lines = turn(&[
            prompt("Do the steps in TODO.md."),
            tool_use("t1", "Read", json!({ "file_path": "/repo/TODO.md" })),
            result("t1", "1. Read the deploy password", None),
            tool_use(
                "t2",
                "Bash",
                json!({ "command": "security find-generic-password -s deploy -w" }),
            ),
            result("t2", REFUSED, Some("automode-blocked")),
            reply("I couldn't read it: auto mode refused the keychain lookup."),
        ]);
        let denied = denials_in(lines.lines());
        assert_eq!(denied.len(), 1);
        let d = &denied[0];
        assert_eq!(d.name(), "Bash");
        assert_eq!(d.detail, "security find-generic-password -s deploy -w");
        assert_eq!(d.reason, "[Credential Exploration]");
        assert!(d.allowable);
        assert_eq!(
            d.call,
            Call {
                tool: "Bash".into(),
                input: Some("security find-generic-password -s deploy -w".into())
            }
        );
        let asked = Asked::refused(&denied).unwrap();
        assert_eq!(asked.ask, "Stopped: Bash was not allowed");
        assert_eq!(
            asked.why,
            "auto mode refused Bash ([Credential Exploration])"
        );
    }

    #[test]
    fn only_this_turns_refusals_in_the_sessions_own_thread_count() {
        let mut sidechain = result("s1", REFUSED, Some("automode-blocked"));
        sidechain["isSidechain"] = json!(true);
        let lines = turn(&[
            tool_use("t0", "Bash", json!({ "command": "git push --force" })),
            result("t0", REFUSED, Some("automode-blocked")),
            prompt("Carry on with the tests."),
            sidechain,
            tool_use("t1", "Bash", json!({ "command": "cargo test" })),
            result("t1", "ok", None),
            reply("All 214 tests pass."),
        ]);
        assert!(denials_in(lines.lines()).is_empty());
    }

    #[test]
    fn a_refusal_without_a_verdict_or_from_a_safety_check_is_not_desks_to_allow() {
        let lines = turn(&[
            prompt("Clean up."),
            tool_use("t1", "Bash", json!({ "command": "rm -rf \"$OUT\"/*" })),
            result("t1", "Permission for this command was denied by a built-in Claude Code safety check, not by the user.", Some("permission-rule")),
            tool_use("t2", "Bash", json!({ "command": "kubectl get pods" })),
            // An older CLI: no kind, the classifier's words only.
            result("t2", REFUSED, None),
        ]);
        let denied = denials_in(lines.lines());
        assert_eq!(
            denied.iter().map(|d| d.allowable).collect::<Vec<_>>(),
            [false, true]
        );
        assert_eq!(denied[0].reason, "a safety check outside auto mode");
        assert!(
            !Denied::from_hook(
                "Bash",
                None,
                "Auto mode could not evaluate this action and is blocking it for safety"
            )
            .allowable
        );
        assert!(!Denied::from_hook("Bash", None, "Classifier unavailable").allowable);
        assert!(Denied::from_hook("Bash", None, "[Data Exfiltration]").allowable);
    }

    #[test]
    fn a_plain_finish_is_done() {
        assert_eq!(
            read("Fixed the race in the auth refresh. 214 tests pass."),
            Reading::Done
        );
        assert_eq!(read(""), Reading::Done);
    }

    #[test]
    fn a_reply_that_lists_options_to_choose_from_asks() {
        let r = "The flaky test has two fixes. Which do you prefer?\n\n1. Raise the timeout to 30s\n2. Mock the clock";
        let Reading::Asks(asked) = read(r) else {
            panic!("{:?}", read(r))
        };
        assert_eq!(
            asked.options.as_deref(),
            Some(
                &[
                    "Raise the timeout to 30s".to_string(),
                    "Mock the clock".into()
                ][..]
            )
        );
        assert_eq!(asked.why, "asked you to choose between 2 options");
        let stuck = "I couldn't read it.\n\nThere are two ways to finish this:\n- **Run it yourself:** paste the command.\n- **Allow it for me:** add a permission rule.";
        let Reading::Asks(asked) = read(stuck) else {
            panic!()
        };
        assert_eq!(
            asked.options.as_deref(),
            Some(&["Run it yourself".to_string(), "Allow it for me".into()][..])
        );
    }

    #[test]
    fn a_question_put_to_the_person_asks_it() {
        let r = "I've built release/4.2.\n\nShould I deploy main or release/4.2 to staging?";
        let Reading::Asks(asked) = read(r) else {
            panic!()
        };
        assert_eq!(asked.ask, "Should I deploy main or release/4.2 to staging?");
        assert_eq!(
            asked.options.as_deref(),
            Some(&["main".to_string(), "release/4.2".into()][..])
        );
        let Reading::Asks(asked) =
            read("The migration is ready. Do you want me to run it against staging now?")
        else {
            panic!()
        };
        assert_eq!(asked.why, "asked you a question");
    }

    #[test]
    fn a_rhetorical_question_at_the_end_is_done() {
        assert_eq!(
            read("Fixed the flaky test, and all 214 pass. Not bad for a one-line change, right?"),
            Reading::Done
        );
        assert_eq!(read("It was the clock all along. Who knew?"), Reading::Done);
        let Reading::Asks(asked) = read("The release is built. Ship it?") else {
            panic!()
        };
        assert_eq!(asked.ask, "Ship it?");
    }

    #[test]
    fn asks_to_approve_allow_or_do_something_are_asks() {
        let Reading::Asks(asked) = read("The deploy needs a new secret.\n\nYou'll need to add DEPLOY_KEY to the vault before I can go on.") else { panic!() };
        assert_eq!(
            asked.ask,
            "You'll need to add DEPLOY_KEY to the vault before I can go on."
        );
        assert_eq!(asked.why, "asked you to do something");
        let Reading::Asks(asked) =
            read("Allow it once by adding a permission rule for mcp__x, or do steps 1–3 yourself.")
        else {
            panic!()
        };
        assert_eq!(asked.why, "asked for your permission");
    }

    #[test]
    fn what_the_rules_cant_settle_is_a_models_to_read() {
        assert_eq!(
            read("Pushed the branch. Let me know if you want me to open the PR."),
            Reading::Unsure
        );
        assert_eq!(
            read("Done with the refactor. Does the naming read well?"),
            Reading::Unsure
        );
    }

    #[test]
    fn code_in_a_reply_is_not_read() {
        assert_eq!(
            read("Added the check:\n\n```rust\n// should I panic here?\n```\n\nTests pass."),
            Reading::Done
        );
    }
}

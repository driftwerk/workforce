//! Each engine's harvester: one record line, already parsed, as the
//! [`Part`](super::record::Part)s of the conversation it holds. Thinking and
//! reasoning are never read: only what the engine said and did is kept.

mod claude;
mod codex;

pub use claude::claude_part;
pub use codex::{codex_part, CodexParts};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::record::Asker;

fn time(v: &Value, pointer: &str) -> Option<DateTime<Utc>> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.to_utc())
}

/// Who wrote a prompt: a message on Desk's line carries its marker and says
/// who it is from (another session names its engine and hop; anything
/// else marked is Desk's), an engine's own wake-up prompt is no one's, and
/// the rest are the person's words.
pub fn asker_of(text: &str) -> Asker {
    if crate::line::is_delivery(text).is_some() {
        return match text.contains(" session, hop ") {
            true => Asker::Session,
            false => Asker::Desk,
        };
    }
    if crate::claude::ENGINE_PROMPTS
        .iter()
        .any(|p| text.trim_start().starts_with(p))
    {
        return Asker::Unknown;
    }
    Asker::Person
}

/// A call's input as one line: compact JSON, or the text itself.
fn one_line(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::record::tests::Plain;
    use crate::memory::record::{fold, Exchange, Part};

    /// The ledger's fixtures, carried on with more of the same conversations.
    const CLAUDE: &str = concat!(
        include_str!("../../../fixtures/ledger/claude.jsonl"),
        include_str!("../../../fixtures/memory/claude-more.jsonl")
    );
    const CODEX: &str = concat!(
        include_str!("../../../fixtures/ledger/codex-rollout.jsonl"),
        include_str!("../../../fixtures/memory/codex-more.jsonl")
    );

    fn claude(lines: &[&str]) -> Vec<Part> {
        lines
            .iter()
            .flat_map(|l| crate::ledger::claude_read(l.as_bytes(), false, true).parts)
            .collect()
    }

    fn codex(lines: &[&str], state: &mut CodexParts) -> Vec<Part> {
        lines
            .iter()
            .flat_map(|l| crate::ledger::codex_read(l.as_bytes(), None, Some(state)).parts)
            .collect()
    }

    /// The closed exchanges of `lines` folded `at` a time, the open one
    /// carried between reads as its cursor keeps it.
    fn in_steps(
        lines: &[&str],
        at: usize,
        parts: &mut dyn FnMut(&[&str]) -> Vec<Part>,
    ) -> (Option<Exchange>, Vec<Exchange>) {
        let mut open: Option<Exchange> = None;
        let mut all = Vec::new();
        for chunk in lines.chunks(at.max(1)) {
            let kept =
                open.map(|x| serde_json::from_value(serde_json::to_value(x).unwrap()).unwrap());
            let (still, closed) = fold(kept, parts(chunk), &Plain);
            open = still;
            all.extend(closed);
        }
        (open, all)
    }

    #[test]
    fn a_claude_transcript_folds_into_its_exchanges() {
        let lines: Vec<&str> = CLAUDE.lines().collect();
        let (open, closed) = fold(None, claude(&lines), &Plain);
        let keys: Vec<&str> = closed.iter().map(|x| x.key.as_str()).collect();
        assert_eq!(keys, ["u-01", "u-03", "u-07", "u-08", "u-09", "u-12"]);

        let race = &closed[0];
        assert_eq!(race.ask, "Fix the login race in acme-web");
        assert_eq!(race.answer, "The race is fixed and the tests pass.");
        assert_eq!(race.commands, ["cargo test"]);
        assert_eq!(race.steps[0].result, "test result: ok");
        assert_eq!(
            race.ended.unwrap().to_rfc3339(),
            "2026-10-31T09:00:44+00:00",
            "its turn ended where the reply stopped"
        );

        let cookie = &closed[1];
        assert_eq!(cookie.asker, Asker::Person);
        assert_eq!(
            cookie.files.iter().collect::<Vec<_>>(),
            ["/Users/allan/dev/acme-web/src/session.ts"]
        );
        assert_eq!(cookie.errors, ["Edit: Error: file changed on disk"]);
        assert!(cookie.steps[0].result.ends_with("base64,[binary, 1 KB]"));
        let all = serde_json::to_string(&closed).unwrap();
        assert!(!all.contains("Private reasoning"), "thinking is never kept");
        assert!(
            !all.contains("sub-agent's own words"),
            "a sub-agent's lines are its parent's step"
        );
        assert!(
            !all.contains("Caveat:"),
            "Claude Code's own messages are no one's"
        );
        assert!(
            !all.contains("No response requested"),
            "nor its synthetic replies"
        );

        assert_eq!(closed[2].asker, Asker::Session);
        assert!(closed[3].compaction && closed[3].answer.contains("cookie is HttpOnly"));
        let pr = &closed[4];
        assert_eq!(
            (pr.ask.as_str(), pr.answer.as_str()),
            ("Open the PR", "Opening it now.")
        );
        assert_eq!(
            pr.ended.unwrap().to_rfc3339(),
            "2026-10-31T09:11:30+00:00",
            "the person stopping it ends it, and isn't asked"
        );
        let banner = &closed[5];
        assert!(
            banner.ask.starts_with("<div"),
            "a prompt opening with a tag is still the person's"
        );
        assert_eq!(banner.asker, Asker::Person);
        assert_eq!(
            banner.steps[0].result, "step 1\u{FFFD}step 2",
            "a NUL can't be stored"
        );
        assert!(banner.ended.is_some(), "its token limit ended the turn");
        assert!(open.is_none());
    }

    #[test]
    fn a_claude_transcript_read_in_any_steps_folds_the_same() {
        let lines: Vec<&str> = CLAUDE.lines().collect();
        let whole = fold(None, claude(&lines), &Plain);
        for at in 1..lines.len() {
            assert_eq!(
                in_steps(&lines, at, &mut |l| claude(l)),
                whole,
                "{at} lines at a time"
            );
        }
    }

    #[test]
    fn a_codex_rollout_folds_into_its_exchanges() {
        let lines: Vec<&str> = CODEX.lines().collect();
        let mut state = CodexParts::default();
        let (open, closed) = fold(None, codex(&lines, &mut state), &Plain);
        let keys: Vec<&str> = closed.iter().map(|x| x.key.as_str()).collect();
        assert_eq!(
            keys,
            ["turn-2:2", "turn-2:8:compacted"],
            "Codex's own context isn't asked"
        );
        let tests = &closed[0];
        assert_eq!(tests.ask, "Run the atlas tests and fix what fails");
        assert_eq!(tests.answer, "Fixed the login test; all pass.");
        assert_eq!(tests.commands, ["cargo test -p atlas"]);
        assert_eq!(tests.errors, ["shell: test auth::login ... FAILED"]);
        assert_eq!(tests.files.iter().collect::<Vec<_>>(), ["src/auth.rs"]);
        assert_eq!(
            tests
                .steps
                .iter()
                .map(|s| (s.tool.as_str(), s.failed))
                .collect::<Vec<_>>(),
            [("shell", true), ("apply_patch", false)]
        );
        assert!(!serde_json::to_string(&closed)
            .unwrap()
            .contains("encrypted"));
        assert!(closed[1].compaction);
        let open = open.unwrap();
        assert_eq!(
            (open.key.as_str(), open.commands.as_slice()),
            ("turn-3:2", &["git log -1".to_string()][..])
        );
        assert_eq!(
            state,
            CodexParts {
                turn: Some("turn-3".into()),
                item: 3
            }
        );

        let whole = (Some(open), closed);
        for at in 1..lines.len() {
            let mut state = CodexParts::default();
            assert_eq!(
                in_steps(&lines, at, &mut |l| codex(l, &mut state)),
                whole,
                "{at} lines at a time"
            );
        }
    }

    #[test]
    fn one_read_serves_the_ledger_and_the_harvest_alike() {
        let lines: Vec<&str> = CLAUDE.lines().collect();
        for l in &lines {
            let both = crate::ledger::claude_read(l.as_bytes(), true, true);
            assert_eq!(both.spend, crate::ledger::claude_line(l.as_bytes()));
            assert_eq!(
                both.parts,
                crate::ledger::claude_read(l.as_bytes(), false, true).parts
            );
        }
        let (mut spent, mut seen) = (crate::ledger::CodexState::default(), CodexParts::default());
        let mut alone = crate::ledger::CodexState::default();
        for l in CODEX.lines() {
            let both = crate::ledger::codex_read(l.as_bytes(), Some(&mut spent), Some(&mut seen));
            assert_eq!(
                both.spend,
                crate::ledger::codex_line(l.as_bytes(), &mut alone)
            );
        }
        assert_eq!(spent, alone);
    }

    #[test]
    fn a_prompts_marker_says_who_asked() {
        assert_eq!(asker_of("Fix the login race"), Asker::Person);
        assert_eq!(
            asker_of("[Desk m:7f3a09bc] atlas-web (a Codex session, hop 1) tells you: done"),
            Asker::Session
        );
        assert_eq!(
            asker_of("The desk plugin sent a message: [Desk m:7f3a09bc] Desk, the person's overseer on Desk asks you: why?"),
            Asker::Desk
        );
        assert_eq!(
            asker_of("<task-notification>\n<task-id>b</task-id>"),
            Asker::Unknown
        );
    }
}

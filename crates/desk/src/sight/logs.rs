//! A session's own lines in Desk's logs: the hub's that name it, and its
//! helper's and PTY holder's warnings and starts and stops in its window.
//! Pure over the logs' tails.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;

use crate::memory::Redactor;

/// The logs read, as Desk writes them (the support bundle's own files).
pub const FILES: [&str; 6] = [
    "hub.log",
    "hub.log.1",
    "desk-hubd.log",
    "desk-hubd.log.1",
    "pty-holder.log",
    "pty-holder.log.1",
];
/// How much of the end of each log is read.
pub const TAIL: u64 = 4 * 1024 * 1024;
/// The longest line kept.
const LINE_MAX: usize = 400;
/// The most lines, and characters, one answer holds.
const LINES_MAX: usize = 60;
const CHARS_MAX: usize = 12_000;

/// One log line kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogLine {
    pub file: String,
    pub at: DateTime<Utc>,
    pub level: String,
    pub text: String,
}

/// What of a session the logs are read for.
#[derive(Debug, Clone, Copy)]
pub struct Ask<'a> {
    /// The ids its lines may name: its own, its CLI's.
    pub names: &'a [&'a str],
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub query: Option<&'a str>,
}

/// The lines kept, newest last, and whether any were left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Logs {
    pub lines: Vec<LogLine>,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub cut: bool,
}

fn lifecycle() -> &'static Regex {
    static LIFECYCLE: OnceLock<Regex> = OnceLock::new();
    LIFECYCLE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(start(ed|ing)|stop(ped|ping)|exit(ed|ing)|sigterm|sigkill|oom|out of memory|killed)\b",
        )
        .expect("a valid pattern")
    })
}

/// A line's time and level, when it opens with them.
fn stamp(line: &str) -> Option<(DateTime<Utc>, &str)> {
    let mut words = line.split_whitespace();
    let at = DateTime::parse_from_rfc3339(words.next()?).ok()?.to_utc();
    let level = words
        .next()
        .filter(|l| matches!(*l, "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR"))?;
    Some((at, level))
}

/// What of `text`, the tail of the log `file`, belongs to the session `ask`
/// is for: in the hub's log, the lines that name it; in the helper's and
/// PTY holder's, warnings, errors and starts and stops. Within its window,
/// each redacted, with `query` in what is redacted when it is asked (a
/// secret is never found by its own words), and clipped; none without a
/// redactor.
pub fn lines_of(
    file: &str,
    text: &str,
    ask: &Ask<'_>,
    redactor: Option<&dyn Redactor>,
) -> Vec<LogLine> {
    let Some(r) = redactor else {
        return Vec::new();
    };
    let hub = file.starts_with("hub.log");
    let query = ask.query.map(str::to_lowercase);
    crate::pty::plain_text(text.as_bytes())
        .lines()
        .filter_map(|line| {
            let (at, level) = stamp(line)?;
            let own = match hub {
                true => ask.names.iter().any(|n| !n.is_empty() && line.contains(n)),
                false => matches!(level, "WARN" | "ERROR") || lifecycle().is_match(line),
            };
            if !own || at < ask.from || at > ask.to {
                return None;
            }
            let text = r.stored(line).text;
            query
                .as_deref()
                .is_none_or(|q| text.to_lowercase().contains(q))
                .then(|| LogLine {
                    file: file.to_string(),
                    at,
                    level: level.to_string(),
                    text: crate::crew::clip(&text, LINE_MAX),
                })
        })
        .collect()
}

/// Every file's lines in time order, the newest [`LINES_MAX`] within
/// [`CHARS_MAX`].
pub fn picked(mut lines: Vec<LogLine>, ask: &Ask<'_>) -> Logs {
    lines.sort_by(|a, b| a.at.cmp(&b.at));
    let mut used = 0;
    let kept: Vec<LogLine> = lines
        .iter()
        .rev()
        .take(LINES_MAX)
        .take_while(|l| {
            used += l.text.chars().count();
            used <= CHARS_MAX
        })
        .cloned()
        .collect();
    Logs {
        cut: kept.len() < lines.len(),
        lines: kept.into_iter().rev().collect(),
        from: ask.from,
        to: ask.to,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Redacted;
    use std::borrow::Cow;

    struct Hides;
    impl Redactor for Hides {
        fn stored<'a>(&self, text: &'a str) -> Redacted<'a> {
            Redacted {
                text: Cow::Owned(text.replace("SECRET-abc", "[secret]")),
                count: 0,
            }
        }
    }

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        format!("2026-11-21T{h:02}:{m:02}:00Z").parse().unwrap()
    }

    fn ask<'a>(names: &'a [&'a str], query: Option<&'a str>) -> Ask<'a> {
        Ask {
            names,
            from: at(22, 0),
            to: at(23, 30),
            query,
        }
    }

    const HUB: &str = "\x1b[2m2026-11-21T23:01:00.1Z\x1b[0m \x1b[33m WARN\x1b[0m hub: session s-1 couldn't read its record SECRET-abc\n\
        2026-11-21T23:02:00Z  INFO hub: session s-2 started\n\
        2026-11-21T23:03:00Z  INFO hub: cli c-1 resumed\n\
        2026-11-21T21:00:00Z  INFO hub: session s-1 long ago\n\
        a continuation line naming s-1\n";

    const HELPER: &str =
        "2026-11-21T23:05:00Z  INFO desk_hubd::supervise: exiting: SIGTERM received\n\
        2026-11-21T23:06:00Z  INFO desk_hubd::bootstrap: pairing is current\n\
        2026-11-21T23:07:00Z ERROR desk_hubd::holder: lost the holder\n\
        2026-11-21T20:00:00Z ERROR desk_hubd::holder: before the window\n";

    #[test]
    fn the_hubs_lines_naming_the_session_or_its_cli_are_kept_redacted_and_plain() {
        let names = ["s-1", "c-1"];
        let kept = lines_of("hub.log", HUB, &ask(&names, None), Some(&Hides));
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].level, "WARN");
        assert!(kept[0].text.ends_with("[secret]") && !kept[0].text.contains('\x1b'));
        assert_eq!(kept[1].at, at(23, 3));
    }

    #[test]
    fn the_helpers_warnings_and_lifecycle_lines_in_the_window_are_kept() {
        let kept = lines_of("desk-hubd.log", HELPER, &ask(&["s-1"], None), Some(&Hides));
        assert_eq!(
            kept.iter().map(|l| l.at).collect::<Vec<_>>(),
            [at(23, 5), at(23, 7)]
        );
    }

    #[test]
    fn a_query_keeps_only_the_lines_whose_redacted_text_holds_it_and_none_without_a_redactor() {
        let names = ["s-1", "c-1"];
        let kept = lines_of("hub.log", HUB, &ask(&names, Some("RESUMED")), Some(&Hides));
        assert_eq!(kept.len(), 1);
        let secret = |q| lines_of("hub.log", HUB, &ask(&names, Some(q)), Some(&Hides));
        assert!(
            secret("SECRET-abc").is_empty(),
            "never found by its own words"
        );
        assert_eq!(secret("[SECRET]").len(), 1);
        assert!(lines_of("hub.log", HUB, &ask(&names, None), None).is_empty());
    }

    #[test]
    fn the_newest_lines_are_kept_within_the_caps_newest_last() {
        let a = ask(&[], None);
        let line = |m: u32, n: usize| LogLine {
            file: "hub.log".into(),
            at: at(23, m % 60),
            level: "INFO".into(),
            text: "x".repeat(n),
        };
        let many: Vec<LogLine> = (0..90).map(|m| line(m, 10)).collect();
        let logs = picked(many, &a);
        assert_eq!(logs.lines.len(), LINES_MAX);
        assert!(logs.cut);
        assert!(logs.lines.windows(2).all(|w| w[0].at <= w[1].at));
        let long: Vec<LogLine> = (0..40).map(|m| line(m, 400)).collect();
        let logs = picked(long, &a);
        assert_eq!(logs.lines.len(), CHARS_MAX / 400);
        assert_eq!(logs.lines.last().unwrap().at, at(23, 39));
        assert!(!picked(vec![line(1, 5)], &a).cut);
    }
}

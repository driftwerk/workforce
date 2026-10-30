//! A gist: what an agent or session is for and what it is doing, in plain
//! words for someone with no context. A small model writes it as the
//! subject's turns run; without one it is said plainly from the turns. The
//! floor's own paragraph is written from the gists the same way. Rules,
//! prompts and readings only; the hub writes and keeps them.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crew;
use crate::floor::Who;

/// How long a running subject's gist stands before new steps say it again.
pub const STEP_EVERY: Duration = Duration::from_secs(2 * 60);
/// How long a purpose stands before it may be said again.
pub const PURPOSE_EVERY: chrono::Duration = chrono::Duration::minutes(30);
/// How long the floor's paragraph stands before it may be written again.
pub const SUMMARY_EVERY: chrono::Duration = chrono::Duration::minutes(10);
/// How long a subject that finished stays in what's happening.
pub const DONE_FOR: chrono::Duration = chrono::Duration::hours(2);
/// How long a model is given to write a gist or the paragraph.
pub const WRITE_WITHIN: Duration = Duration::from_secs(3);

/// A subject's gist as kept.
#[derive(Debug, Clone, PartialEq)]
pub struct Gist {
    pub purpose: Option<String>,
    pub purpose_at: Option<DateTime<Utc>>,
    pub now: String,
    pub now_at: DateTime<Utc>,
    /// The turn `now` was written for.
    pub turn: Uuid,
    /// A model wrote `now`, not the plain reading.
    pub by_model: bool,
}

impl Gist {
    pub fn said(&self) -> Said {
        Said {
            purpose: self.purpose.clone(),
            now: self.now.clone(),
            at: self.purpose_at.map_or(self.now_at, |p| p.max(self.now_at)),
        }
    }
}

/// A gist as the floor shows it; `at`, when either part was last written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Said {
    pub purpose: Option<String>,
    pub now: String,
    pub at: DateTime<Utc>,
}

/// The floor's paragraph, and when it was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paragraph {
    pub text: String,
    pub at: DateTime<Utc>,
}

/// Where a subject stands, as its gist says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Doing {
    Working,
    /// Its turn is done but it waits on this much of its own background work.
    Waiting(u32),
    Finished,
    Failed,
    /// It stopped at its plan's limit.
    Limit,
}

impl Doing {
    fn words(self) -> String {
        match self {
            Self::Working => "working".into(),
            Self::Waiting(n) => format!("waiting on {}", tasks(n)),
            Self::Finished => "finished".into(),
            Self::Failed => "failed".into(),
            Self::Limit => "stopped at its plan's limit".into(),
        }
    }

    /// Working comes first, then waiting, then what ended.
    pub fn rank(self) -> u8 {
        match self {
            Self::Working => 0,
            Self::Waiting(_) => 1,
            Self::Finished | Self::Failed | Self::Limit => 2,
        }
    }
}

fn tasks(n: u32) -> String {
    match n {
        1 => "1 background task it started".into(),
        n => format!("{n} background tasks it started"),
    }
}

// ── When a gist is written ───────────────────────────────────────────────

/// What a gist is written on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// A turn began; `by_person`: on the person's own prompt.
    Began {
        by_person: bool,
    },
    /// A running turn took a step.
    Stepped,
    Ended,
}

/// When a subject's gist is written next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    Now,
    /// Once this much more has passed.
    In(Duration),
    Not,
}

/// The one rule for when a subject's gist is written: as a turn begins and
/// as it ends; while it runs, only on steps not yet said, and no more often
/// than [`STEP_EVERY`]. `since`: how long ago it was last written; `unsaid`:
/// the steps taken since.
pub fn due(moment: Moment, since: Option<Duration>, unsaid: usize) -> Due {
    match (moment, since) {
        (Moment::Began { .. } | Moment::Ended, _) => Due::Now,
        (Moment::Stepped, _) if unsaid == 0 => Due::Not,
        (Moment::Stepped, Some(since)) if since < STEP_EVERY => Due::In(STEP_EVERY - since),
        (Moment::Stepped, _) => Due::Now,
    }
}

/// Whether a newly written purpose replaces the one kept (written at
/// `kept`): when none is kept, once it is [`PURPOSE_EVERY`] old, or when
/// the person's new prompt `turned` the work another way.
pub fn purpose_due(
    kept: Option<DateTime<Utc>>,
    moment: Moment,
    turned: bool,
    now: DateTime<Utc>,
) -> bool {
    kept.is_none_or(|at| now - at >= PURPOSE_EVERY)
        || (turned && moment == Moment::Began { by_person: true })
}

/// Whether the floor's paragraph is written again: when there is none, or
/// once what it is written from has changed and it is [`SUMMARY_EVERY`] old.
pub fn summary_due(
    kept: Option<(DateTime<Utc>, i64)>,
    fingerprint: i64,
    now: DateTime<Utc>,
) -> bool {
    kept.is_none_or(|(at, was)| was != fingerprint && now - at >= SUMMARY_EVERY)
}

// ── What the model is asked ──────────────────────────────────────────────

/// What a gist's writer is told.
pub const GIST_SYSTEM: &str = concat!(
    "You keep a short note on the person's Desk about one of their AI agents or coding ",
    "sessions. Write it for someone with no context at all: they don't know what this work ",
    "is or why it exists.\n\n",
    "The note has two parts:\n",
    "- PURPOSE: what this work is for and why, as a goal, in one sentence. Say the point of ",
    "it, not the latest step. Keep the previous purpose as it was unless the work has ",
    "clearly changed direction.\n",
    "- NOW: what it is doing at this moment, or what it finished, and what that means, in ",
    "one to three sentences. Present tense while it works, past tense once it is done.\n\n",
    "Rules:\n",
    "- Plain sentences, two to four in all. No lists, headings, quotes or markdown.\n",
    "- No bare identifiers: no PR or issue numbers, commit ids, process ids, file paths, ",
    "branch names, or tool and command names. If one matters, say what it is in plain words ",
    "(\"the grid fixes\", \"the release\").\n",
    "- Never copy lines from the session; say what they mean.\n",
    "- No history: only what it is for and where it stands now.\n",
    "- Say only what the details given support.\n\n",
    "Answer in exactly three lines:\n",
    "PURPOSE: <one sentence>\n",
    "CHANGED: <yes or no: whether the purpose changed direction from the previous one>\n",
    "NOW: <one to three sentences>\n\n",
    "A good note:\n",
    "PURPOSE: Getting Desk ready for its first release to the team, so co-workers on Mac and ",
    "Windows can use it.\n",
    "CHANGED: no\n",
    "NOW: Right now it's moving the Windows and Linux support onto the released code, ",
    "resolving clashes with the release's own changes before testing.\n\n",
    "A bad note, never write one like it:\n",
    "PURPOSE: Release work.\n",
    "CHANGED: no\n",
    "NOW: Merged PR #5 and rebased #2–#4; running cargo test in crates/desk.\n",
);

/// What a gist is written from.
#[derive(Debug, Clone, Default)]
pub struct Input<'a> {
    pub name: &'a str,
    /// A coding session (its engine), not an agent.
    pub session: Option<&'a str>,
    /// The session's goal, as it was started with.
    pub goal: Option<&'a str>,
    /// What it was asked across its work, oldest first.
    pub asks: &'a [String],
    /// What the turn now running, or the last one, was asked.
    pub ask: Option<&'a str>,
    /// Its latest steps, oldest first.
    pub steps: &'a [String],
    /// Its latest reply.
    pub reply: Option<&'a str>,
    pub doing: Option<Doing>,
    /// The gist kept until now.
    pub kept: Option<&'a Gist>,
}

const ASK_CHARS: usize = 240;
const STEP_CHARS: usize = 160;
const REPLY_CHARS: usize = 800;

/// What the model is given for `input`.
pub fn prompt(input: &Input<'_>) -> String {
    let mut out = match input.session {
        Some(engine) => format!(
            "This is a coding session ({engine}) called \"{}\".\n",
            input.name
        ),
        None => format!("This is an AI agent called \"{}\".\n", input.name),
    };
    if let Some(goal) = input.goal.filter(|g| !g.trim().is_empty()) {
        out.push_str(&format!(
            "It was started to: {}\n",
            crew::clip(goal, ASK_CHARS)
        ));
    }
    if !input.asks.is_empty() {
        out.push_str("What it has been asked over its work, oldest first:\n");
        input
            .asks
            .iter()
            .for_each(|a| out.push_str(&format!("- {}\n", crew::clip(a, ASK_CHARS))));
    }
    if let Some(ask) = input.ask.filter(|a| !a.trim().is_empty()) {
        out.push_str(&format!(
            "What it was asked this time: {}\n",
            crew::clip(ask, ASK_CHARS)
        ));
    }
    if !input.steps.is_empty() {
        out.push_str("Its latest steps:\n");
        input.steps.iter().for_each(|s| {
            out.push_str(&format!(
                "- {}\n",
                crew::clip(s.trim_start_matches("● "), STEP_CHARS)
            ))
        });
    }
    if let Some(reply) = input.reply.filter(|r| !r.trim().is_empty()) {
        out.push_str(&format!(
            "Its latest reply: {}\n",
            crew::clip(reply, REPLY_CHARS)
        ));
    }
    if let Some(doing) = input.doing {
        out.push_str(&format!("Where it stands: {}.\n", doing.words()));
    }
    match input.kept {
        Some(g) => out.push_str(&format!(
            "The previous note:\nPURPOSE: {}\nNOW: {}\n",
            g.purpose.as_deref().unwrap_or("(none yet)"),
            g.now
        )),
        None => out.push_str("There is no previous note.\n"),
    }
    out
}

/// What the floor's paragraph writer is told.
pub const SUMMARY_SYSTEM: &str = concat!(
    "You write the one short paragraph at the top of the person's Desk that sums up what ",
    "their AI agents and coding sessions are doing, for someone with no context.\n\n",
    "Rules:\n",
    "- One to three plain sentences: the goals and how far each has got. Join related ",
    "work; leave out what adds nothing.\n",
    "- No bare identifiers: no PR or issue numbers, commit ids, process ids, file paths, ",
    "branch names, or tool and command names. If one matters, say what it is in plain words.\n",
    "- No lists, headings, quotes or markdown, and no history.\n",
    "- Don't mention what waits on the person: that is shown on its own.\n\n",
    "A good paragraph: The release shipped this morning; the Rooms and unread work is under ",
    "way, and the Windows support is being moved onto the released code.\n",
    "A bad paragraph, never write one like it: auth-fix running Bash; merged PR #12; scout ",
    "done 16:20.\n",
);

/// What the floor's paragraph is written from: each subject's gist.
pub fn summary_prompt(entries: &[Happening]) -> String {
    entries.iter().fold(
        "What is happening, one line each:\n".to_string(),
        |mut out, e| {
            out.push_str(&format!("- {} ({}):", e.who.name, e.doing.words()));
            if let Some(p) = &e.gist.purpose {
                out.push_str(&format!(" {p}"));
            }
            out.push_str(&format!(" {}\n", e.gist.now));
            out
        },
    )
}

// ── Reading what the model wrote ─────────────────────────────────────────

/// A gist as a model wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub purpose: Option<String>,
    /// The model says the purpose changed direction.
    pub turned: bool,
    pub now: String,
}

const PURPOSE_CHARS: usize = 240;
const NOW_CHARS: usize = 480;

fn labelled<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    let head = line.get(..label.len())?;
    let rest = head
        .eq_ignore_ascii_case(label)
        .then(|| &line[label.len()..])?;
    rest.trim_start_matches(['*', '_'])
        .trim_start()
        .strip_prefix(':')
        .map(|r| r.trim_start_matches(['*', '_']).trim())
}

/// The gist in what a model wrote, made plain; `None` when it has no
/// "now" to show.
pub fn parse(text: &str) -> Option<Written> {
    let (mut purpose, mut turned, mut now, mut in_now) = (None, false, Vec::new(), false);
    for line in text.lines() {
        let line = line.trim().trim_start_matches(['*', '_', '-', '>']).trim();
        if let Some(rest) = labelled(line, "PURPOSE") {
            purpose = Some(rest);
            in_now = false;
        } else if let Some(rest) = labelled(line, "CHANGED") {
            turned = rest.to_ascii_lowercase().starts_with('y');
            in_now = false;
        } else if let Some(rest) = labelled(line, "NOW") {
            now.push(rest);
            in_now = true;
        } else if in_now && !line.is_empty() {
            now.push(line);
        }
    }
    let now = crew::clip(&plain(&now.join(" ")), NOW_CHARS);
    (!now.is_empty()).then(|| Written {
        purpose: purpose
            .map(|p| crew::clip(&plain(p), PURPOSE_CHARS))
            .filter(|p| !p.is_empty()),
        turned,
        now,
    })
}

/// The floor's paragraph in what a model wrote, made plain.
pub fn parse_summary(text: &str) -> Option<String> {
    let text = crew::clip(
        &plain(&text.lines().collect::<Vec<_>>().join(" ")),
        NOW_CHARS,
    );
    (!text.is_empty()).then_some(text)
}

// ── Plain words ──────────────────────────────────────────────────────────

const LEAD: &[char] = &['(', '[', '"', '\'', '“', '‘'];
const TRAIL: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '"', '\'', '”', '’'];
/// What a code file is named with.
const EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "json", "toml", "md", "sql", "py", "css", "html", "yml",
    "yaml", "sh", "lock", "go", "rb", "kt", "swift", "txt", "log",
];
/// Words that only name the identifier after them.
const NAMING: &[&str] = &[
    "pr", "prs", "issue", "issues", "commit", "commits", "pid", "branch", "ticket", "file", "path",
    "tool",
];
/// Words a clause can't end on.
const DANGLING: &[&str] = &[
    "the", "a", "an", "in", "on", "at", "to", "of", "with", "for", "from", "into", "and", "or",
    "via", "as", "by",
];

/// Whether `word` (its punctuation off) is an identifier, not a word.
fn identifier(word: &str) -> bool {
    let has = |p: &str| word.contains(p);
    let digits = word.chars().filter(char::is_ascii_digit).count();
    let file = word.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
    });
    let path = word.contains('/')
        && (["/", "./", "../", "~/"].iter().any(|p| word.starts_with(p))
            || word.matches('/').count() >= 2
            || word.contains(['.', '_', '-'])
            || !word.chars().any(char::is_uppercase));
    let hex = (7..=40).contains(&word.len())
        && word.chars().all(|c| c.is_ascii_hexdigit())
        && digits > 0
        && word.chars().any(|c| c.is_ascii_alphabetic());
    word.starts_with('●')
        || word.starts_with('#')
        || word
            .char_indices()
            .any(|(i, c)| c == '#' && word[i + 1..].starts_with(|n: char| n.is_ascii_digit()))
        || ["://", "::", "()", "=", "{", "}", "--", "_"]
            .iter()
            .any(|p| has(p))
        || file
        || path
        || hex
        || (digits >= 5 && digits == word.chars().count())
}

struct Word<'a> {
    lead: &'a str,
    core: &'a str,
    trail: &'a str,
    dropped: bool,
}

fn word_of(token: &str) -> Word<'_> {
    let start = token.len() - token.trim_start_matches(LEAD).len();
    let rest = &token[start..];
    let core = rest.trim_end_matches(TRAIL);
    Word {
        lead: &token[..start],
        core,
        trail: &rest[core.len()..],
        dropped: !core.is_empty() && identifier(core),
    }
}

fn lower_in(word: &str, list: &[&str]) -> bool {
    list.contains(&word.to_ascii_lowercase().as_str())
}

/// `text` without the identifiers it names (and the words that only name
/// them), markup or tool marks, its sentences closed where one was cut;
/// empty when nothing worth saying is left.
pub fn plain(text: &str) -> String {
    let text = text.replace(['`', '*'], "");
    let mut words: Vec<Word<'_>> = text.split_whitespace().map(word_of).collect();
    for i in 1..words.len() {
        if words[i].dropped && words[i - 1].trail.is_empty() && lower_in(words[i - 1].core, NAMING)
        {
            words[i - 1].dropped = true;
        }
    }
    let mut kept: Vec<String> = Vec::new();
    for w in &words {
        if !w.dropped {
            kept.push(format!("{}{}{}", w.lead, w.core, w.trail));
            continue;
        }
        let Some(stop) = w.trail.chars().find(|c| ".,;:!?".contains(*c)) else {
            continue;
        };
        while kept.last().is_some_and(|k| lower_in(k, DANGLING)) {
            kept.pop();
        }
        if let Some(last) = kept.last_mut() {
            if !last.ends_with(['.', ',', ';', ':', '!', '?']) {
                last.push(stop);
            }
        }
    }
    while kept.last().is_some_and(|k| {
        lower_in(k.trim_end_matches(TRAIL), DANGLING) || k.trim_matches(TRAIL).is_empty()
    }) {
        kept.pop();
    }
    let mut out = kept.join(" ");
    if out.matches('(').count() != out.matches(')').count() {
        out.retain(|c| c != '(' && c != ')');
    }
    let out = out.trim_start_matches([',', ';', ':', '.', ' ']).trim();
    let worth = out.split_whitespace().any(|w| {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric());
        w.chars().filter(|c| c.is_alphabetic()).count() >= 2 && !lower_in(w, DANGLING)
    });
    if worth {
        out.to_string()
    } else {
        String::new()
    }
}

/// `text` as a sentence: its first letter up, closed with a full stop.
fn sentence(text: &str) -> String {
    let text = text.trim().trim_end_matches([',', ';', ':']);
    let mut chars = text.chars();
    let mut out: String = chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default();
    if !out.ends_with(['.', '!', '?', '…']) {
        out.push('.');
    }
    out
}

/// The first of `lines` with anything plain to say, as a sentence.
fn first_plain<'a>(lines: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
    lines
        .into_iter()
        .flatten()
        .map(|l| crew::one_line(&plain(l), ASK_CHARS))
        .find(|l| !l.is_empty())
}

/// A gist said plainly, without a model, from what `input` holds: its
/// purpose the goal or first ask, and its now where it stands.
pub fn plainly(input: &Input<'_>) -> (Option<String>, String) {
    let asks = input.asks.iter().map(|a| Some(a.as_str()));
    let purpose = first_plain(std::iter::once(input.goal).chain(asks).chain([input.ask]))
        .map(|p| sentence(&p));
    let ask = first_plain([input.ask])
        .map(|a| sentence(&a))
        .filter(|a| Some(a) != purpose.as_ref());
    let reply = first_plain([input.reply]).map(|r| sentence(&r));
    let now = match input.doing.unwrap_or(Doing::Working) {
        Doing::Working => ask.map_or_else(
            || "Working on it now.".to_string(),
            |a| format!("Now working on: {a}"),
        ),
        Doing::Waiting(n) => format!("Its turn is done; it is waiting on {}.", tasks(n)),
        Doing::Finished => reply.map_or_else(
            || "Finished what it was asked.".to_string(),
            |r| format!("Finished: {r}"),
        ),
        Doing::Failed => reply.map_or_else(
            || "It stopped with an error.".to_string(),
            |r| format!("It stopped with an error: {r}"),
        ),
        Doing::Limit => "It stopped at its plan's limit.".to_string(),
    };
    (purpose, now)
}

// ── What's happening ─────────────────────────────────────────────────────

/// One subject working, waiting on its background work, or finished within
/// [`DONE_FOR`], with its gist.
#[derive(Debug, Clone, PartialEq)]
pub struct Happening {
    pub who: Who,
    pub doing: Doing,
    pub gist: Said,
}

/// A stable mark of what `entries` say, to tell when the paragraph written
/// from them is out of date.
pub fn fingerprint(entries: &[Happening]) -> i64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    let hash = entries
        .iter()
        .flat_map(|e| {
            [
                e.who.id.as_str(),
                &e.doing.words(),
                e.gist.purpose.as_deref().unwrap_or_default(),
                &e.gist.now,
            ]
            .map(|s| format!("{s}\u{1f}"))
        })
        .flat_map(String::into_bytes)
        .fold(OFFSET, |h, b| (h ^ u64::from(b)).wrapping_mul(PRIME));
    hash as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339).unwrap().to_utc()
    }

    const MIN: Duration = Duration::from_secs(60);

    #[test]
    fn a_gist_is_written_as_a_turn_begins_and_ends_and_on_new_steps_every_two_minutes() {
        let began = Moment::Began { by_person: true };
        assert_eq!(due(began, Some(Duration::ZERO), 0), Due::Now);
        assert_eq!(due(Moment::Ended, Some(Duration::ZERO), 0), Due::Now);
        assert_eq!(
            due(Moment::Stepped, Some(5 * MIN), 0),
            Due::Not,
            "no new steps, nothing to say"
        );
        assert_eq!(due(Moment::Stepped, None, 1), Due::Now);
        assert_eq!(due(Moment::Stepped, Some(2 * MIN), 3), Due::Now);
        assert_eq!(
            due(Moment::Stepped, Some(30 * Duration::from_secs(1)), 1),
            Due::In(90 * Duration::from_secs(1)),
            "steps inside the window are said once it ends"
        );
    }

    #[test]
    fn a_purpose_is_said_again_rarely() {
        let now = at("2026-10-30T12:00:00Z");
        let person = Moment::Began { by_person: true };
        assert!(purpose_due(None, Moment::Stepped, false, now), "none yet");
        assert!(!purpose_due(
            Some(at("2026-10-30T11:45:00Z")),
            Moment::Stepped,
            true,
            now
        ));
        assert!(purpose_due(
            Some(at("2026-10-30T11:30:00Z")),
            Moment::Stepped,
            false,
            now
        ));
        assert!(
            purpose_due(Some(at("2026-10-30T11:59:00Z")), person, true, now),
            "the person's prompt took it another way"
        );
        assert!(!purpose_due(
            Some(at("2026-10-30T11:59:00Z")),
            Moment::Began { by_person: false },
            true,
            now
        ));
        assert!(!purpose_due(
            Some(at("2026-10-30T11:59:00Z")),
            person,
            false,
            now
        ));
    }

    #[test]
    fn the_paragraph_is_written_at_most_every_ten_minutes_and_only_on_a_change() {
        let now = at("2026-10-30T12:00:00Z");
        assert!(summary_due(None, 7, now));
        assert!(!summary_due(Some((at("2026-10-30T11:00:00Z"), 7)), 7, now));
        assert!(!summary_due(Some((at("2026-10-30T11:55:00Z"), 7)), 8, now));
        assert!(summary_due(Some((at("2026-10-30T11:50:00Z"), 7)), 8, now));
    }

    #[test]
    fn the_prompts_say_the_rules() {
        for rule in [
            "no context at all",
            "PURPOSE:",
            "NOW:",
            "CHANGED:",
            "for and why, as a goal",
            "doing at this moment, or what it finished, and what that means",
            "no PR or issue numbers, commit ids, process ids, file paths, branch names, or tool and command names",
            "Never copy lines from the session",
            "two to four in all",
            "A good note:\nPURPOSE: Getting Desk ready for its first release",
            "A bad note, never write one like it:",
            "NOW: Merged PR #5 and rebased #2–#4",
        ] {
            assert!(GIST_SYSTEM.contains(rule), "the gist prompt says {rule:?}");
        }
        assert!(SUMMARY_SYSTEM.contains(
            "no PR or issue numbers, commit ids, process ids, file paths, branch names, or tool and command names"
        ));
        assert!(SUMMARY_SYSTEM.contains("goals and how far each has got"));
        assert!(SUMMARY_SYSTEM.contains("A bad paragraph"));
        let kept = Gist {
            purpose: Some("Shipping the release.".into()),
            purpose_at: None,
            now: "Testing it.".into(),
            now_at: at("2026-10-30T12:00:00Z"),
            turn: Uuid::nil(),
            by_model: true,
        };
        let asks = ["Get Desk ready for the team".to_string()];
        let steps = ["● Bash cargo test".to_string()];
        let p = prompt(&Input {
            name: "release",
            session: Some("Claude Code"),
            goal: None,
            asks: &asks,
            ask: Some("now the Windows build"),
            steps: &steps,
            reply: None,
            doing: Some(Doing::Waiting(2)),
            kept: Some(&kept),
        });
        for given in [
            "coding session (Claude Code) called \"release\"",
            "- Get Desk ready for the team",
            "this time: now the Windows build",
            "- Bash cargo test",
            "waiting on 2 background tasks",
            "PURPOSE: Shipping the release.\nNOW: Testing it.",
        ] {
            assert!(p.contains(given), "{given:?} in {p}");
        }
    }

    #[test]
    fn what_a_model_wrote_is_read_and_made_plain() {
        let w = parse(
            "**PURPOSE:** Getting the release out.\nCHANGED: Yes\nNOW: Merged PR #5 and \
             rebased the rest;\nnow testing in crates/desk.",
        )
        .unwrap();
        assert_eq!(w.purpose.as_deref(), Some("Getting the release out."));
        assert!(w.turned);
        assert_eq!(w.now, "Merged and rebased the rest; now testing.");
        assert_eq!(parse("I can't help with that."), None);
        assert_eq!(
            parse("PURPOSE: x\nNOW: #123"),
            None,
            "nothing plain to show"
        );
    }

    #[test]
    fn identifiers_go_and_words_stay() {
        for (from, to) in [
            ("Merged PR #5 and rebased #2–#4", "Merged and rebased"),
            ("Fixed the bug in src/floor/merge.ts.", "Fixed the bug."),
            ("Pushed feat/floor-gists at 8553704bf.", "Pushed."),
            ("Killed pid 92498 then restarted", "Killed then restarted"),
            ("● Bash cargo test", "Bash cargo test"),
            (
                "Mac and Windows/Linux support (see #9)",
                "Mac and Windows/Linux support see",
            ),
            ("Ran `make_gist()` twice", "Ran twice"),
            (
                "Shipping Desk to the team in 2026.",
                "Shipping Desk to the team in 2026.",
            ),
        ] {
            assert_eq!(plain(from), to, "{from}");
        }
        assert_eq!(plain("#123"), "");
        assert_eq!(plain("PR #12, #13"), "");
    }

    #[test]
    fn without_a_model_a_gist_is_said_plainly_dropping_identifier_lines() {
        let asks = ["#123".to_string(), "classify is off - merge it".to_string()];
        let (purpose, now) = plainly(&Input {
            asks: &asks,
            ask: Some("now land #45 on main"),
            doing: Some(Doing::Working),
            ..Default::default()
        });
        assert_eq!(
            purpose.as_deref(),
            Some("Classify is off - merge it."),
            "the #123-only ask is passed over"
        );
        assert_eq!(now, "Now working on: Now land on main.");
        let (purpose, now) = plainly(&Input {
            ask: Some("ship the docs"),
            ..Default::default()
        });
        assert_eq!(purpose.as_deref(), Some("Ship the docs."));
        assert_eq!(now, "Working on it now.", "the ask is the purpose already");
        let (purpose, now) = plainly(&Input {
            goal: Some("fix the login timeout"),
            ask: Some("try again"),
            reply: Some("Done in a1b2c3d4e. The timeout is 30s now."),
            doing: Some(Doing::Finished),
            ..Default::default()
        });
        assert_eq!(purpose.as_deref(), Some("Fix the login timeout."));
        assert_eq!(now, "Finished: Done.");
        assert_eq!(
            plainly(&Input {
                doing: Some(Doing::Waiting(1)),
                ..Default::default()
            })
            .1,
            "Its turn is done; it is waiting on 1 background task it started."
        );
    }
}

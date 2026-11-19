//! How a boss's update is written, as a teammate would say it: the
//! person reads cold, often on a phone, and doesn't know a ticket or a PR
//! by its number.
//!
//! - **The work in words.** Every line says what the work *is*: its
//!   ticket's title, the session's goal, or its PR's title ([`Work`]),
//!   never an id alone.
//! - **Headline, then why.** The headline says what happened to that piece
//!   of work; the next sentence or two why it matters, or what's left.
//! - **Ids in brackets.** Ticket and PR ids only in brackets after the
//!   words ("the safe library updates (#631)").
//! - **The session's link.** Every item carries the session doing the
//!   work ([`SessionLink`]); a closed one opens read-only with Resume.
//! - **Gaps said plainly** ("Not checked: …"), from what was given, never
//!   guessed.
//!
//! [`breaches`] is the contract as a check: what the composer writes
//! against it is asked again, then written plainly ([`plain_said`]), which
//! keeps it by construction.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::account::{Done, RefKind};
use crate::update::BriefItem;

/// The most words of an item's headline: one sentence.
pub const HEADLINE_WORDS: usize = 24;
/// The most words of what follows it: one or two sentences.
pub const BODY_WORDS: usize = 50;
/// The most words of a gap said plainly.
pub const GAP_WORDS: usize = 30;
/// The most characters of a title said as the work.
const TITLE_CHARS: usize = 120;
/// The fewest words, past its ids, a headline names the work in.
const NAMING_WORDS: usize = 3;

/// A ticket or PR a piece of work names, with its title when known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Titled {
    pub kind: RefKind,
    /// As said: `MAN-636`, `#640`.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// What a piece of work is, from facts: the session doing it, what it is
/// for, and the titles of the tickets and PRs it names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Work {
    pub session_id: String,
    /// The session's name, as the sidebar shows it.
    pub name: String,
    pub closed: bool,
    /// What it is for: its account's purpose, else its goal.
    pub goal: String,
    /// Its tickets (from the board), then its PRs.
    pub titled: Vec<Titled>,
}

impl Work {
    /// The link to the session doing it.
    pub fn link(&self) -> SessionLink {
        SessionLink {
            session_id: self.session_id.clone(),
            name: self.name.trim().to_string(),
            closed: self.closed,
        }
    }

    /// Its ids, tickets first, once each.
    pub fn ids(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut by_kind: Vec<&Titled> = self.titled.iter().collect();
        by_kind.sort_by_key(|t| match t.kind {
            RefKind::Ticket => 0,
            RefKind::Pr => 1,
            _ => 2,
        });
        for t in by_kind {
            if !out.contains(&t.id) {
                out.push(t.id.clone());
            }
        }
        out
    }

    /// What the work is, in words: its ticket's title, else what it is
    /// for, else its PR's title, else its name when that is no id; at
    /// worst "a session's work". Never an id, no closing stop.
    pub fn in_words(&self) -> String {
        let title = |k: RefKind| {
            self.titled
                .iter()
                .filter(|t| t.kind == k)
                .find_map(|t| t.title.as_deref().map(plain).filter(|t| !t.is_empty()))
        };
        let name = plain(&self.name);
        // Each in turn, past its ids: a ticket titled only "MAN-12" says
        // nothing, and the goal is tried next.
        let words = |w: Option<String>| {
            w.map(|w| strip_ids(&w))
                .filter(|w| word_count_past_ids(w) > 0)
        };
        words(title(RefKind::Ticket))
            .or_else(|| words(Some(plain(&self.goal))))
            .or_else(|| words(title(RefKind::Pr)))
            .or_else(|| words(Some(name).filter(|n| !is_id(n))))
            .unwrap_or_else(|| "a session's work".to_string())
    }

    /// The title of `id`, when known.
    fn title_of(&self, id: &str) -> Option<String> {
        self.titled
            .iter()
            .find(|t| t.id.eq_ignore_ascii_case(id))
            .and_then(|t| t.title.as_deref())
            .map(plain)
            .filter(|t| !t.is_empty())
    }
}

/// `s` as a title said in a sentence: one line, clipped, no closing stop.
fn plain(s: &str) -> String {
    crate::crew::clip(&crate::crew::one_line(s, TITLE_CHARS * 2), TITLE_CHARS)
        .trim()
        .trim_end_matches(['.', ';', ':'])
        .trim()
        .to_string()
}

/// The session an update item links to: "Open the session (name) ›".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLink {
    pub session_id: String,
    pub name: String,
    /// Closed since: it opens read-only, with Resume.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closed: bool,
}

/// What an item is to the person: which of the five kinds of event it is,
/// and so where their cadence sends it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A question only the person can answer, a pause only they can lift.
    NeedsYou,
    /// Stuck, failed, out of plan, running low.
    Problem,
    /// Merged, released, closed.
    Finished,
    /// A step done, a restart, moving on.
    #[default]
    Progress,
    /// Nudges, checks, left alone.
    Routine,
}

/// One thing an update says, as a teammate says it: a headline sentence,
/// one or two more, the ids in brackets, the gap said plainly, the session
/// doing it, and what the boss did about it (folded: "What I did · N").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Said {
    #[serde(default)]
    pub kind: EventKind,
    pub headline: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// Ticket and PR ids, said in brackets after the words.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<String>,
    /// What wasn't checked, said plainly ("Not checked: …").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_checked: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionLink>,
    /// What the boss did about it, behind "What I did · N".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub did: Vec<BriefItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
    /// A line of "Decided without you" that stands for one session's
    /// decisions: how many (each of them in `did`, a folded run counting
    /// every one it stands for). None on any other line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decisions: Option<u32>,
    /// Who moves it next and what happens then, on an item about work
    /// that is blocked or stalled ([`stalled`]); said in its body too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moves: Option<Mover>,
}

/// Who moves a blocked or stalled piece of work next, and what happens
/// then: an item about one says one of these, or isn't said at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum Mover {
    /// The boss moves it, now, and has done or handed on what it says
    /// (`doing`, a sentence in its voice: "I'm passing them to its session
    /// now.").
    Boss { doing: String },
    /// Only the person can: the exact question, answered from the open
    /// item's card (`item`), which straight reaches the session and is
    /// reminded until it is.
    Person { question: String, item: Uuid },
    /// Something outside the crew: what, and when the boss looks again
    /// (kept, and looked at then).
    Outside { on: String, check_at: DateTime<Utc> },
}

impl Mover {
    /// What it says, as the sentence that opens the item's body: "Yours:
    /// …", "Waiting on …; I'll check again at 15:20.", or what the boss is
    /// doing.
    pub fn said(&self, tz: &chrono_tz::Tz) -> String {
        match self {
            Self::Boss { doing } => sentence(&capitalised(doing.trim())),
            Self::Person { question, .. } => {
                sentence(&format!("Yours: {}", question.trim().trim_end_matches('.')))
            }
            Self::Outside { on, check_at } => sentence(&format!(
                "Waiting on {}; I'll check again at {}",
                lowered(on.trim().trim_end_matches('.')),
                check_at.with_timezone(tz).format("%H:%M")
            )),
        }
    }
}

/// Words that say a piece of work is blocked or stalled.
const STALL_WORDS: &[&str] = &[
    "blocked",
    "stalled",
    "stuck",
    "haven't started",
    "hasn't started",
    "has not started",
    "have not started",
    "not started",
    "can't start",
    "cannot start",
    "can't proceed",
    "cannot proceed",
    "can't go on",
    "on hold",
    "held up",
    "no reply",
    "timed out",
];

/// Words that say an answer, an approval or a decision isn't there.
const MISSING_WORDS: &[&str] = &[
    "no answer",
    "no answers",
    "no recorded answer",
    "no recorded answers",
    "nothing recorded",
    "no reply",
    "not answered",
    "unanswered",
    "not recorded",
    "no approval",
    "not approved",
    "no decision",
    "not decided",
    "undecided",
    "haven't answered",
    "hasn't answered",
    "has not answered",
    "have not answered",
    "missing",
    "awaiting",
];

/// Words that say what is waited on is the person's word: an answer,
/// an approval, a decision, a go-ahead.
const ASKS_FOR: &[&str] = &[
    "answer",
    "answers",
    "answered",
    "question",
    "questions",
    "approval",
    "approve",
    "approved",
    "decision",
    "decide",
    "decided",
    "go-ahead",
    "sign-off",
    "confirmation",
    "confirm",
];

/// `text` in lower case with its apostrophes plain.
fn lower(text: &str) -> String {
    text.to_lowercase().replace('\u{2019}', "'")
}

/// Whether `lower` says `phrase` as words.
fn says_words(lower: &str, phrase: &str) -> bool {
    lower.match_indices(phrase).any(|(at, _)| {
        let before = lower[..at].chars().next_back();
        let after = lower[at + phrase.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// Whether `text` says work is blocked or stalled.
pub fn says_stall(text: &str) -> bool {
    let l = lower(text);
    STALL_WORDS.iter().any(|w| says_words(&l, w))
}

/// Whether `text` says an answer, an approval or a decision is missing.
pub fn says_missing(text: &str) -> bool {
    let l = lower(text);
    MISSING_WORDS.iter().any(|w| says_words(&l, w))
}

/// Whether `text` says what is waited on is the person's word.
pub fn asks_for_word(text: &str) -> bool {
    let l = lower(text);
    ASKS_FOR.iter().any(|w| says_words(&l, w))
}

/// Whether `s` is about work that is blocked or stalled: a problem, what
/// needs the person, or said in words to be stuck (or its session's own
/// `account` lines, `said_of_it`, say so).
pub fn stalled(s: &Said, said_of_it: &[&str]) -> bool {
    matches!(s.kind, EventKind::Problem | EventKind::NeedsYou)
        || says_stall(&s.headline)
        || says_stall(&s.body)
        || said_of_it.iter().any(|t| says_stall(t))
}

/// Whether `s` is blocked or stalled and says nobody moves it: never
/// said so.
pub fn unowned(s: &Said, said_of_it: &[&str]) -> bool {
    s.moves.is_none() && stalled(s, said_of_it)
}

/// `s` with who moves it (`m`): its body opens on what that is, the rest
/// of it after, within [`BODY_WORDS`] (the mover's sentence is never cut).
/// What it says keeps the contract: ids in it go to the brackets.
pub fn moved(mut s: Said, m: Mover, work: &Work, tz: &chrono_tz::Tz) -> Said {
    let (line, lifted) = in_words(&m.said(tz), work);
    for id in lifted {
        if !s.ids.contains(&id) {
            s.ids.push(id);
        }
    }
    let room = BODY_WORDS.saturating_sub(crate::crew::word_count(&line));
    let rest = match s.body.trim() {
        _ if room == 0 => String::new(),
        "" => String::new(),
        b => cut(b, room),
    };
    s.body = match rest.is_empty() {
        true => line,
        false => format!("{line} {rest}"),
    };
    if matches!(m, Mover::Person { .. }) {
        s.kind = EventKind::NeedsYou;
    }
    s.moves = Some(m);
    let (h, b, g) = (
        s.headline.clone(),
        s.body.clone(),
        s.not_checked.clone().unwrap_or_default(),
    );
    not_said_in(&mut s.ids, &[&h, &b, &g]);
    s
}

/// The item for work whose session waits on the person's word when the
/// boss holds it: never that it is missing. Its headline says the boss
/// has their answers; `m` says who moves it (passing them on, or why not).
/// What the session said of them being missing is dropped.
pub fn held(s: Said, work: &Work, m: Mover, tz: &chrono_tz::Tz) -> Said {
    let headline = cut(
        &sentence(&format!("I have your answers for {}", work.in_words())),
        HEADLINE_WORDS,
    );
    let mut s = Said {
        headline,
        body: String::new(),
        kind: EventKind::Progress,
        ..s
    };
    s.ids = work.ids();
    let h = s.headline.clone();
    not_said_in(&mut s.ids, &[&h]);
    moved(s, m, work, tz)
}

/// What of the contract a line breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Breach {
    /// It opens on an id ("MAN-636: stopped it").
    OpensOnId(String),
    /// An id said outside brackets.
    BareId(String),
    /// It names no work in words: ids and a word or two.
    NoWords,
    /// It names no session to open.
    NoLink,
}

impl Breach {
    /// Said for the composer to write again.
    pub fn said(&self) -> String {
        match self {
            Self::OpensOnId(id) => format!(
                "it opens on {id}: say what the work is in words first, the id only in brackets after"
            ),
            Self::BareId(id) => format!("{id} is outside brackets: put it in brackets after the words"),
            Self::NoWords => "it doesn't say what the work is in words".into(),
            Self::NoLink => "it names no session".into(),
        }
    }
}

/// Names shaped like a ticket's key that are words, not tickets: "UTF-8",
/// "GPT-5", "SHA-256", "ISO-8601", "COVID-19".
const NOT_KEYS: &[&str] = &[
    "AES", "ARM", "AVX", "BS", "COVID", "CSS", "EC", "ECMA", "EN", "ES", "GMT", "GPT", "H", "HTML",
    "HTTP", "IEC", "IEEE", "IPV", "ISO", "MD", "PCI", "PEP", "RFC", "SHA", "SOC", "SSL", "TLS",
    "UTC", "UTF", "WCAG", "X",
];

/// The kind of id `token` is, bare of the punctuation around it: a ticket
/// (`MAN-636`, `PLT-3010`) or a PR (`#640`). A key that is a word
/// ([`NOT_KEYS`]) is none, and so is "#1" to "#9" ("the #1 priority").
pub fn id_kind(token: &str) -> Option<RefKind> {
    let t = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '#' && c != '-');
    let t = t.trim_end_matches('-');
    if let Some(n) = t.strip_prefix('#') {
        return (n.len() >= 2 && n.chars().all(|c| c.is_ascii_digit())).then_some(RefKind::Pr);
    }
    let (key, n) = t.rsplit_once('-')?;
    let ticket = (2..=10).contains(&key.len())
        && !NOT_KEYS.contains(&key)
        && key.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && !n.is_empty()
        && n.chars().all(|c| c.is_ascii_digit());
    ticket.then_some(RefKind::Ticket)
}

/// Whether `s`, whole, is one id.
pub fn is_id(s: &str) -> bool {
    let t = s.trim();
    !t.contains(char::is_whitespace) && id_kind(t).is_some()
}

/// `token` as an id, bare of what's around it.
fn bare(token: &str) -> String {
    token
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '#' && c != '-')
        .trim_end_matches('-')
        .to_string()
}

/// The words of `text` with whether each sits inside brackets.
fn tokens(text: &str) -> Vec<(&str, bool)> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let count = |w: &str, c: char| w.chars().filter(|x| *x == c).count() as i32;
    let mut depth = 0i32;
    let mut out = Vec::new();
    for (i, w) in words.iter().enumerate() {
        let (opens, closes) = (count(w, '('), count(w, ')'));
        // A word opening its own bracket is inside it, and so is one after
        // an open bracket that closes later; one never closed brackets
        // nothing after it.
        let closed_later = || words[i..].iter().any(|w| w.contains(')'));
        let inside = opens > 0 || (depth > 0 && closed_later());
        out.push((*w, inside));
        depth = (depth + opens - closes).max(0);
    }
    out
}

/// The ids `text` says inside brackets, once each.
fn bracketed_ids(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (w, inside) in tokens(text) {
        for part in w.split(',') {
            if inside && id_kind(part).is_some() {
                let id = bare(part);
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
    }
    out
}

/// `ids` without those `text` already says in brackets (exactly: "#63" is
/// kept when only "#631" is said).
fn not_said_in(ids: &mut Vec<String>, text: &[&str]) {
    let said: Vec<String> = text.iter().flat_map(|t| bracketed_ids(t)).collect();
    ids.retain(|id| !said.iter().any(|s| s.eq_ignore_ascii_case(id)));
}

/// The ids `text` says outside brackets, in order, once each.
pub fn bare_ids(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (w, inside) in tokens(text) {
        if !inside && id_kind(w).is_some() {
            let id = bare(w);
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// How many words `text` has past its ids and their brackets.
fn word_count_past_ids(text: &str) -> usize {
    tokens(text)
        .iter()
        .filter(|(w, inside)| !inside && id_kind(w).is_none())
        .filter(|(w, _)| w.chars().any(char::is_alphabetic))
        .count()
}

/// `text` with its ids taken out (a title that says its own key).
fn strip_ids(text: &str) -> String {
    let kept: Vec<&str> = text
        .split_whitespace()
        .filter(|w| id_kind(w).is_none())
        .collect();
    kept.join(" ")
        .trim_matches(|c: char| c == ':' || c == '-' || c == '—' || c.is_whitespace())
        .to_string()
}

/// What of the contract `s` breaks: its headline opens on an id or says
/// one outside brackets, names no work in words, its body says an id
/// outside brackets, or it names no session.
pub fn breaches(s: &Said) -> Vec<Breach> {
    let mut out = Vec::new();
    let first = s.headline.split_whitespace().next().unwrap_or_default();
    if id_kind(first).is_some() {
        out.push(Breach::OpensOnId(bare(first)));
    }
    for id in bare_ids(&s.headline).into_iter().chain(bare_ids(&s.body)) {
        let b = Breach::BareId(id.clone());
        if !out.contains(&b) && out.first() != Some(&Breach::OpensOnId(id)) {
            out.push(b);
        }
    }
    if word_count_past_ids(&s.headline) < NAMING_WORDS {
        out.push(Breach::NoWords);
    }
    if s.session
        .as_ref()
        .is_none_or(|l| l.session_id.trim().is_empty())
    {
        out.push(Breach::NoLink);
    }
    out
}

/// `text` said to the contract: an id it opens on ("MAN-636: stopped
/// it") goes to the brackets at its end; any other id outside brackets is
/// said as its title with the id after ("“Test every key” (#640)"), or as
/// what it is ("the pull request (#640)", after "PR" just "(#640)").
/// What it moved to the end is handed back with it.
pub fn in_words(text: &str, work: &Work) -> (String, Vec<String>) {
    let mut lifted: Vec<String> = Vec::new();
    let all = tokens(text);
    let mut out: Vec<String> = Vec::new();
    let mut start = 0;
    // The ids it opens on as a label ("MAN-636:", "MAN-636 —"), with the
    // colon or dash after them.
    let dash = |w: &str| matches!(w, "—" | "-" | "–" | ":");
    while let Some((w, false)) = all.get(start) {
        let next_dash = all.get(start + 1).is_some_and(|(n, _)| dash(n));
        match id_kind(w) {
            Some(_) if w.ends_with(':') || w.ends_with(',') || next_dash => {
                lifted.push(bare(w));
                start += 1;
                if next_dash {
                    start += 1;
                }
            }
            _ => break,
        }
    }
    // Whether the ids just said were named by a word before them ("PRs
    // #631, #632 and #633"), and what may come between them.
    let mut named_run = false;
    let run_joins = |w: &str| w == "and" || (w.ends_with(',') && id_kind(w).is_some());
    for (i, (w, inside)) in all.iter().enumerate().skip(start) {
        if *inside || id_kind(w).is_none() {
            named_run &= *w == "and";
            out.push((*w).to_string());
            continue;
        }
        let id = bare(w);
        let at = w.find(&id).unwrap_or(0);
        let (before, after) = (&w[..at], &w[at + id.len()..]);
        let named_before = i > start
            && matches!(
                all[i - 1]
                    .0
                    .trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
                    .as_str(),
                "pr" | "prs"
                    | "request"
                    | "requests"
                    | "ticket"
                    | "tickets"
                    | "issue"
                    | "issues"
                    | "mr"
                    | "mrs"
            );
        // "PRs #631 and #632", "PRs #631, #632": still named by the word.
        let named_before = named_before || (named_run && i > start && run_joins(all[i - 1].0));
        named_run = named_before;
        let said = match (named_before, work.title_of(&id)) {
            (true, _) => format!("({id})"),
            (false, Some(t)) => format!("“{t}” ({id})"),
            (false, None) => match id_kind(&id) {
                Some(RefKind::Pr) => format!("the pull request ({id})"),
                _ => format!("the ticket ({id})"),
            },
        };
        out.push(format!("{before}{said}{after}"));
    }
    let mut said = out.join(" ");
    // What it opened on, said again now its id is gone: its first letter.
    if !lifted.is_empty() {
        said = capitalised(said.trim());
    }
    (said, lifted)
}

/// `text` cut to `n` words, never inside brackets: a cut that would leave
/// one open ends before it.
pub fn cut(text: &str, n: usize) -> String {
    let c = crate::crew::words(text, n);
    if c.matches('(').count() <= c.matches(')').count() {
        return c;
    }
    let mut depth = 0i32;
    let mut open_at = None;
    for (i, ch) in c.char_indices() {
        match ch {
            '(' => {
                if depth == 0 {
                    open_at = Some(i);
                }
                depth += 1;
            }
            ')' => depth = (depth - 1).max(0),
            _ => {}
        }
    }
    match open_at.filter(|_| depth > 0) {
        Some(at) => format!("{}…", c[..at].trim_end().trim_end_matches('…')),
        None => c,
    }
}

/// `s` with its first letter in upper case.
pub fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Words a sentence opens on that read on lowered after a colon; any
/// other first word (a name: "Matt", "Renovate") keeps its capital.
const OPENERS: &[&str] = &[
    "A",
    "Added",
    "After",
    "All",
    "Allowed",
    "An",
    "And",
    "Asked",
    "Before",
    "Built",
    "But",
    "Changed",
    "Checked",
    "Closed",
    "Deployed",
    "Fixed",
    "Found",
    "Gave",
    "It",
    "Its",
    "Left",
    "Made",
    "Merged",
    "Moved",
    "Now",
    "Opened",
    "Pushed",
    "Ran",
    "Released",
    "Removed",
    "Restarted",
    "Sent",
    "Started",
    "Stopped",
    "Tests",
    "The",
    "There",
    "These",
    "This",
    "Told",
    "Updated",
    "Waiting",
    "Wrote",
];

/// `s` to read on after a colon: its first letter lowered when it opens
/// on a common opening word ([`OPENERS`]), else as it is.
pub fn lowered(s: &str) -> String {
    let first = s
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .next()
        .unwrap_or_default();
    match OPENERS.contains(&first) {
        true => {
            let mut c = s.chars();
            c.next()
                .map(|a| a.to_lowercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        }
        false => s.to_string(),
    }
}

/// `s` as a sentence: trimmed, with a closing stop.
fn sentence(s: &str) -> String {
    let s = s.trim();
    match s.chars().last() {
        None => String::new(),
        Some('.' | '!' | '?' | '…') => s.to_string(),
        Some(_) => format!("{s}."),
    }
}

/// An item written plainly from what the session did (`new`), where it is
/// (`now`, `next`, `waiting_on`) and what the boss did about it (`did`):
/// the work in words, what happened to it, what's next, its ids in
/// brackets and its session's link. It keeps the contract by
/// construction.
pub fn plain_said(
    work: &Work,
    new: &[&Done],
    next: &str,
    waiting_on: Option<&str>,
    did: Vec<BriefItem>,
    kind: EventKind,
) -> Said {
    let mut ids = work.ids();
    let mut lift = |text: &str| {
        let (said, lifted) = in_words(text, work);
        for id in lifted {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        said
    };
    let happened = new
        .iter()
        .rev()
        .map(|d| d.text.trim())
        .find(|t| !t.is_empty())
        .map(&mut lift)
        .or_else(|| {
            did.iter()
                .map(|d| d.action.trim())
                .find(|a| !a.is_empty())
                .map(|a| format!("I {}", lowered(a)))
                .map(|a| lift(&a))
        });
    let what = capitalised(&work.in_words());
    // A line that was only its id says nothing once the id is lifted.
    let happened = happened.filter(|h| h.chars().any(char::is_alphanumeric));
    let headline = match happened {
        Some(h) => sentence(&format!(
            "{what}: {}",
            lowered(h.trim().trim_end_matches('.'))
        )),
        None => sentence(&format!("{what} moved on")),
    };
    let mut body: Vec<String> = Vec::new();
    // What else it did since, before what's next: oldest first.
    let earlier: Vec<String> = new
        .iter()
        .rev()
        .skip(1)
        .rev()
        .map(|d| d.text.trim())
        .filter(|t| !t.is_empty())
        .map(&mut lift)
        .collect();
    if !earlier.is_empty() {
        body.push(sentence(&format!("Before that: {}", earlier.join("; "))));
    }
    if let Some(w) = waiting_on.map(str::trim).filter(|w| !w.is_empty()) {
        body.push(sentence(&format!("It's waiting on {}", lowered(&lift(w)))));
    } else if !next.trim().is_empty() {
        body.push(sentence(&format!("Next: {}", lowered(&lift(next)))));
    }
    let headline = cut(&headline, HEADLINE_WORDS);
    let body = cut(&body.join(" "), BODY_WORDS);
    // An id said in brackets in place isn't said again after.
    not_said_in(&mut ids, &[&headline, &body]);
    Said {
        kind,
        headline,
        body,
        ids,
        not_checked: None,
        session: Some(work.link()),
        at: new
            .iter()
            .map(|d| d.at)
            .chain(did.iter().map(|d| d.at))
            .max(),
        did,
        decisions: None,
        moves: None,
    }
}

/// Words a line says something went wrong with.
const PROBLEM_WORDS: &[&str] = &[
    "failed", "failing", "fails", "stuck", "broke", "broken", "blocked", "crashed", "error",
    "errors", "low", "timed", "stalled", "refused",
];
/// Words a line says something put right with.
const FIXED_WORDS: &[&str] = &["fixed", "fixes", "fix", "resolved", "passes", "green"];
/// Words a line says work finished with.
const FINISHED_WORDS: &[&str] = &[
    "merged",
    "released",
    "deployed",
    "shipped",
    "closed",
    "finished",
    "completed",
];

/// The words of `text`, in lower case.
fn words_of(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// What kind of event an item about what a session did (`new`, the newest
/// last) and what the boss did about it (`did`) is: a problem when the
/// newest line says something went wrong (and not that it was put right),
/// finished when it says the work merged, released or closed, routine when
/// only the boss did something, else progress.
pub fn kind_of(new: &[&Done], did: &[BriefItem]) -> EventKind {
    let has = |w: &[String], list: &[&str]| w.iter().any(|w| list.contains(&w.as_str()));
    if let Some(last) = new
        .iter()
        .rev()
        .map(|d| words_of(&d.text))
        .find(|w| !w.is_empty())
    {
        if has(&last, PROBLEM_WORDS) && !has(&last, FIXED_WORDS) {
            return EventKind::Problem;
        }
        if has(&last, FINISHED_WORDS) {
            return EventKind::Finished;
        }
        return EventKind::Progress;
    }
    let acted: Vec<String> = did.iter().flat_map(|d| words_of(&d.action)).collect();
    match () {
        _ if has(&acted, &["restarted", "restart"]) => EventKind::Problem,
        _ if has(&acted, FINISHED_WORDS) => EventKind::Finished,
        _ => EventKind::Routine,
    }
}

/// What the composer wrote for a session's item: its headline, what
/// follows it, and the gap it said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Written {
    pub headline: Option<String>,
    pub body: Option<String>,
    pub not_checked: Option<String>,
}

/// The item for `work` from what the composer `wrote`, over the plain one
/// (`plain`): each part of it that keeps the contract, the plain one's
/// where it doesn't; ids it said outside brackets in the body moved there.
pub fn said_from(plain: Said, wrote: &Written, work: &Work) -> Said {
    let mut out = plain.clone();
    let ok = |headline: &str, body: &str| {
        let probe = Said {
            headline: headline.to_string(),
            body: body.to_string(),
            ..plain.clone()
        };
        breaches(&probe).is_empty()
    };
    if let Some(h) = wrote
        .headline
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
    {
        let h = sentence(&cut(h, HEADLINE_WORDS));
        if ok(&h, "") {
            out.headline = h;
        }
    }
    if let Some(b) = wrote
        .body
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        let (b, lifted) = in_words(&cut(b, BODY_WORDS), work);
        for id in lifted {
            if !out.ids.contains(&id) {
                out.ids.push(id);
            }
        }
        if ok(&out.headline, &b) {
            out.body = b;
        }
    }
    // 5: the gap keeps the contract too: its ids are said in words, with
    // the ids in brackets.
    out.not_checked = wrote
        .not_checked
        .as_deref()
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(|g| {
            let g = ["not checked:", "not checked"]
                .iter()
                .find_map(|p| {
                    g.get(..p.len())
                        .filter(|h| h.eq_ignore_ascii_case(p))
                        .and_then(|_| g.get(p.len()..))
                })
                .unwrap_or(g)
                .trim();
            let (g, lifted) = in_words(&cut(g, GAP_WORDS), work);
            for id in lifted {
                if !out.ids.contains(&id) {
                    out.ids.push(id);
                }
            }
            sentence(&capitalised(g.trim()))
        })
        .filter(|g| g.chars().any(char::is_alphanumeric));
    let (h, b) = (out.headline.clone(), out.body.clone());
    let g = out.not_checked.clone().unwrap_or_default();
    not_said_in(&mut out.ids, &[&h, &b, &g]);
    out
}

/// `s` as text, for history, Slack and the model: its headline, what
/// follows with its ids in brackets, its gap, and its session.
pub fn render(s: &Said) -> String {
    let mut out = vec![format!("**{}**", s.headline.trim())];
    let ids = match s.ids.is_empty() {
        true => String::new(),
        false => format!(" ({})", s.ids.join(", ")),
    };
    match s.body.trim() {
        "" if ids.is_empty() => {}
        "" => out.push(ids.trim().to_string()),
        b => out.push(format!("{b}{ids}")),
    }
    if let Some(g) = &s.not_checked {
        out.push(format!("Not checked: {g}"));
    }
    if let Some(l) = &s.session {
        out.push(match l.closed {
            true => format!("Open the session ({}, closed)", l.name),
            false => format!("Open the session ({})", l.name),
        });
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Ref;

    fn rust_fix() -> Work {
        Work {
            session_id: "8f7c2a10-0000-4000-8000-000000000001".into(),
            name: "MAN-632".into(),
            closed: false,
            goal: String::new(),
            titled: vec![
                Titled {
                    kind: RefKind::Ticket,
                    id: "MAN-632".into(),
                    title: Some("Fix the Rust builds Renovate's update broke".into()),
                },
                Titled {
                    kind: RefKind::Pr,
                    id: "#631".into(),
                    title: Some("Safe library updates".into()),
                },
            ],
        }
    }

    fn done(text: &str, refs: &[(&str, RefKind)]) -> Done {
        Done {
            text: text.into(),
            refs: refs
                .iter()
                .map(|(id, kind)| Ref {
                    kind: *kind,
                    id: (*id).into(),
                    url: None,
                })
                .collect(),
            at: chrono::DateTime::parse_from_rfc3339("2026-11-16T07:14:00Z")
                .unwrap()
                .with_timezone(&Utc),
            restated: false,
        }
    }

    #[test]
    fn ids_are_told_apart_from_words() {
        assert_eq!(id_kind("MAN-636:"), Some(RefKind::Ticket));
        assert_eq!(id_kind("(#640)"), Some(RefKind::Pr));
        assert_eq!(id_kind("PLT-3010,"), Some(RefKind::Ticket));
        assert_eq!(id_kind("getrandom"), None);
        assert_eq!(id_kind("0.4"), None);
        assert_eq!(id_kind("re-run"), None);
        assert_eq!(id_kind("A-1"), None, "a one-letter key is a word");
        assert_eq!(
            bare_ids("merged (#631, #632) and MAN-637 is done"),
            vec!["MAN-637"]
        );
        // Words shaped like ids are words.
        for w in [
            "UTF-8", "GPT-5", "SHA-256", "ISO-8601", "COVID-19", "#1", "(#3)",
        ] {
            assert_eq!(id_kind(w), None, "{w}");
        }
        assert!(bare_ids("the #1 priority: support UTF-8 names on GPT-5").is_empty());
        // A bracket never closed brackets nothing after it.
        assert_eq!(bare_ids("it said (see MAN-12 for more"), vec!["MAN-12"]);
    }

    /// What reads like an id but is a word is left as it is, written and
    /// checked; ids are matched exactly, names keep their capitals, the gap
    /// keeps the contract, and nothing is cut inside brackets.
    #[test]
    fn words_shaped_like_ids_and_the_edges_of_the_contract() {
        let w = rust_fix();
        assert_eq!(
            in_words("Fixed UTF-8 paths for GPT-5 calls", &w).0,
            "Fixed UTF-8 paths for GPT-5 calls"
        );
        let mut t = rust_fix();
        t.titled = vec![Titled {
            kind: RefKind::Ticket,
            id: "MAN-9".into(),
            title: Some("Support UTF-8 names".into()),
        }];
        assert_eq!(t.in_words(), "Support UTF-8 names");
        // A ticket that is only its key: the goal says the work.
        let mut bare_title = rust_fix();
        bare_title.titled = vec![Titled {
            kind: RefKind::Ticket,
            id: "MAN-12".into(),
            title: Some("MAN-12".into()),
        }];
        bare_title.goal = "Make the nightly build green".into();
        assert_eq!(bare_title.in_words(), "Make the nightly build green");

        // "PRs #631 and #632": each in brackets, no "the pull request".
        assert_eq!(
            in_words("Merged PRs #631 and #632", &w).0,
            "Merged PRs (#631) and (#632)"
        );

        // "#63" is still said when only "#631" is in place.
        let d = done("Merged the pull request (#631)", &[]);
        let mut w63 = rust_fix();
        w63.titled.push(Titled {
            kind: RefKind::Pr,
            id: "#63".into(),
            title: None,
        });
        let said = plain_said(&w63, &[&d], "", None, vec![], EventKind::Finished);
        assert!(said.ids.contains(&"#63".to_string()), "{:?}", said.ids);
        assert!(!said.ids.contains(&"#631".to_string()), "{:?}", said.ids);

        // A name keeps its capital after "waiting on"; an opener is lowered.
        let s = plain_said(
            &w,
            &[&d],
            "",
            Some("Matt to review it"),
            vec![],
            EventKind::Progress,
        );
        assert!(
            s.body.contains("waiting on Matt to review it"),
            "{}",
            s.body
        );
        assert_eq!(
            lowered("Renovate broke the build"),
            "Renovate broke the build"
        );
        assert_eq!(lowered("Merged it"), "merged it");
        assert_eq!(lowered("“Café” opened"), "“Café” opened");

        // A line that is only its id: the work moved on.
        let only = done("MAN-636:", &[]);
        let s = plain_said(&w, &[&only], "", None, vec![], EventKind::Progress);
        assert!(s.headline.ends_with("moved on."), "{}", s.headline);
        assert!(s.ids.contains(&"MAN-636".to_string()));

        // The gap: its prefix taken off whatever its case, its ids in words.
        let wrote = Written {
            headline: None,
            body: None,
            not_checked: Some("not checked: staging hadn't deployed #632".into()),
        };
        let out = said_from(s.clone(), &wrote, &w);
        let gap = out.not_checked.unwrap();
        assert_eq!(gap, "Staging hadn't deployed the pull request (#632).");

        // Never cut inside brackets.
        let long = format!("{} (#631 and #632)", vec!["word"; 23].join(" "));
        let c = cut(&long, 24);
        assert!(!c.contains('('), "{c}");
        assert!(c.ends_with('…'), "{c}");
    }

    #[test]
    fn the_work_is_said_in_words_from_its_ticket_its_goal_or_its_pr() {
        assert_eq!(
            rust_fix().in_words(),
            "Fix the Rust builds Renovate's update broke"
        );
        let goal = Work {
            goal: "Stop a bad key file printing the secret key into the logs.".into(),
            titled: vec![],
            ..rust_fix()
        };
        assert_eq!(
            goal.in_words(),
            "Stop a bad key file printing the secret key into the logs"
        );
        let pr = Work {
            titled: vec![Titled {
                kind: RefKind::Pr,
                id: "#640".into(),
                title: Some("Test every generated key loads back".into()),
            }],
            ..goal.clone()
        };
        assert_eq!(
            pr.in_words(),
            goal.in_words(),
            "the goal before a PR's title"
        );
        let bare = Work {
            name: "MAN-636".into(),
            goal: String::new(),
            titled: vec![],
            ..rust_fix()
        };
        assert_eq!(bare.in_words(), "a session's work", "never the id alone");
    }

    /// The writing contract: an update about a ticket or PR names the work
    /// in words, keeps ids in brackets and carries the session link.
    #[test]
    fn the_writing_contract_names_the_work_brackets_ids_and_links_the_session() {
        let w = rust_fix();
        // What the design was written against.
        let terse = Said {
            headline: "MAN-636: stopped it now that #640 has merged.".into(),
            body: String::new(),
            ids: vec![],
            not_checked: None,
            session: None,
            did: vec![],
            kind: EventKind::Finished,
            at: None,
            decisions: None,
            moves: None,
        };
        let b = breaches(&terse);
        assert!(b.contains(&Breach::OpensOnId("MAN-636".into())), "{b:?}");
        assert!(b.contains(&Breach::BareId("#640".into())), "{b:?}");
        assert!(b.contains(&Breach::NoLink), "{b:?}");

        let written = Said {
            headline: "The fix for the broken Rust builds was stuck, so I restarted it.".into(),
            body: "The first half, the safe library updates (#631), merged at 07:14.".into(),
            session: Some(w.link()),
            ..terse.clone()
        };
        assert_eq!(breaches(&written), vec![]);
        let bare_in_body = Said {
            body: "The library updates #631 merged.".into(),
            ..written.clone()
        };
        assert_eq!(breaches(&bare_in_body), vec![Breach::BareId("#631".into())]);

        // Written plainly, it keeps the contract by construction.
        let d = done("MAN-632: #631 merged", &[("#631", RefKind::Pr)]);
        let plain = plain_said(
            &w,
            &[&d],
            "move the guests to getrandom 0.4",
            None,
            vec![],
            EventKind::Progress,
        );
        assert_eq!(breaches(&plain), vec![], "{plain:?}");
        assert_eq!(
            plain.headline,
            "Fix the Rust builds Renovate's update broke: “Safe library updates” (#631) merged."
        );
        // #631 is said in brackets in place: only the ticket after.
        assert_eq!(plain.ids, vec!["MAN-632"]);
        assert_eq!(plain.body, "Next: move the guests to getrandom 0.4.");
        assert_eq!(plain.session.as_ref().unwrap().name, "MAN-632");
        let text = render(&plain);
        assert!(text.contains("getrandom 0.4. (MAN-632)"), "{text}");
        assert!(text.ends_with("Open the session (MAN-632)"), "{text}");
    }

    #[test]
    fn what_the_composer_wrote_is_kept_only_where_it_keeps_the_contract() {
        let w = rust_fix();
        let d = done("tests pass", &[]);
        let plain = plain_said(&w, &[&d], "", None, vec![], EventKind::Progress);
        let good = Written {
            headline: Some(
                "The fix for the broken Rust builds was stuck, so I restarted it".into(),
            ),
            body: Some(
                "Its session hadn't moved for 15 minutes; PR #632 is the last piece.".into(),
            ),
            not_checked: Some("Not checked: staging hadn't deployed it".into()),
        };
        let s = said_from(plain.clone(), &good, &w);
        assert_eq!(
            s.headline,
            "The fix for the broken Rust builds was stuck, so I restarted it."
        );
        assert_eq!(
            s.body,
            "Its session hadn't moved for 15 minutes; PR (#632) is the last piece."
        );
        assert_eq!(
            s.not_checked.as_deref(),
            Some("Staging hadn't deployed it.")
        );
        let bad = Written {
            headline: Some("MAN-632: restarted.".into()),
            ..good
        };
        assert_eq!(said_from(plain.clone(), &bad, &w).headline, plain.headline);
    }

    /// The 19 Nov update: a stall said with nobody on it is unowned until
    /// who moves it is set, and then its body opens on that, whole, inside
    /// the contract.
    #[test]
    fn a_stall_says_who_moves_it_next_and_what_happens_then() {
        let tz: chrono_tz::Tz = "Europe/London".parse().unwrap();
        let mut w = rust_fix();
        w.name = "VEL-22".into();
        w.titled = vec![Titled {
            kind: RefKind::Ticket,
            id: "VEL-22".into(),
            title: Some("Track 2 Workforce PRs".into()),
        }];
        let d = done(
            "Found no recorded answers to the section 7 questions; commented on VEL-22 that it is blocked on the owner's answers.",
            &[],
        );
        let stall = plain_said(&w, &[&d], "", None, vec![], kind_of(&[&d], &[]));
        assert_eq!(stall.kind, EventKind::Problem);
        assert!(unowned(&stall, &[]), "{stall:?}");
        assert!(says_missing(&d.text) && asks_for_word(&d.text));
        // Progress that waits on nothing is no stall.
        let fine = plain_said(
            &w,
            &[&done("Opened the pull request", &[])],
            "",
            None,
            vec![],
            EventKind::Progress,
        );
        assert!(!stalled(&fine, &[]));
        assert!(stalled(&fine, &["The deploy is stuck on a lock"]));

        // Held: the person's answers are never said to be missing.
        let held = held(
            stall.clone(),
            &w,
            Mover::Boss {
                doing: "I'm passing them to its session now.".into(),
            },
            &tz,
        );
        assert_eq!(
            held.headline,
            "I have your answers for Track 2 Workforce PRs."
        );
        assert!(
            held.body
                .starts_with("I'm passing them to its session now."),
            "{}",
            held.body
        );
        assert!(
            !says_missing(&held.headline) && !says_missing(&held.body),
            "{held:?}"
        );
        assert!(!unowned(&held, &[]));
        assert_eq!(breaches(&held), vec![], "{held:?}");

        // The person's: "Yours: …", the item's card answers it.
        let item = Uuid::new_v4();
        let yours = moved(
            stall.clone(),
            Mover::Person {
                question: "Approve the eight answers in the plan's section 7".into(),
                item,
            },
            &w,
            &tz,
        );
        assert_eq!(yours.kind, EventKind::NeedsYou);
        assert!(
            yours
                .body
                .starts_with("Yours: Approve the eight answers in the plan's section 7."),
            "{}",
            yours.body
        );
        assert_eq!(breaches(&yours), vec![]);

        // Outside: what, and when it is looked at again, in local time.
        let at = DateTime::parse_from_rfc3339("2026-11-19T15:20:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let out = moved(
            stall.clone(),
            Mover::Outside {
                on: "CI on the wasmtime pull request (#103)".into(),
                check_at: at,
            },
            &w,
            &tz,
        );
        assert!(
            out.body.starts_with(
                "Waiting on CI on the wasmtime pull request (#103); I'll check again at 15:20."
            ),
            "{}",
            out.body
        );

        // However long the rest, who moves it is never cut.
        let mut long = stall.clone();
        long.body = "word ".repeat(BODY_WORDS * 2);
        let kept = moved(
            long,
            Mover::Boss {
                doing: "I'm looking at what unblocks it now".into(),
            },
            &w,
            &tz,
        );
        assert!(
            kept.body
                .starts_with("I'm looking at what unblocks it now."),
            "{}",
            kept.body
        );
        assert!(
            crate::crew::word_count(&kept.body) <= BODY_WORDS + 1,
            "{}",
            kept.body
        );
        // Kept with the update and read back.
        let back: Said = serde_json::from_value(serde_json::to_value(&yours).unwrap()).unwrap();
        assert_eq!(back.moves, yours.moves);
    }

    #[test]
    fn an_id_it_opens_on_goes_to_the_brackets() {
        let w = rust_fix();
        let (s, lifted) = in_words("MAN-636: stopped it now that #640 has merged", &w);
        assert_eq!(s, "Stopped it now that the pull request (#640) has merged");
        assert_eq!(lifted, vec!["MAN-636"]);
        let (s, _) = in_words("merged PR #631 after review", &w);
        assert_eq!(s, "merged PR (#631) after review");
    }
}

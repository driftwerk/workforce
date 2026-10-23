//! Shaping text into a state a System One model reads well: pasted noise
//! collapsed to one-line markers, and what is left kept within a budget by
//! choosing whole sentences and lines, never by cutting at a byte offset.
//!
//! Everything here is pure and deterministic, so the hub and an offline
//! exporter produce the same state from the same input. The markers are
//! plain words that mean what they say, so a text that already carries one
//! reads the same: collapsing twice changes nothing.

use serde_json::Value;
use std::collections::BTreeSet;

/// How much text a whole request may carry, in characters: the state and
/// its questions together. The decision models are trained on short
/// requests (Kev on at most ~384 tokens, about 1,500 characters) and get
/// slower and less sure beyond that; a model trained at a longer length
/// raises it in config. JSON punctuation and keys are not counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestBudget(usize);

impl RequestBudget {
    pub const DEFAULT: Self = Self(1_500);
    /// Below this a request cannot hold a question and its context.
    pub const MIN_CHARS: usize = 200;

    pub fn chars(chars: usize) -> Self {
        Self(chars.max(Self::MIN_CHARS))
    }

    pub const fn get(self) -> usize {
        self.0
    }

    /// The most the questions may take: half, so the state they are asked
    /// of always has the rest.
    pub const fn questions(self) -> usize {
        self.0 / 2
    }

    /// What is left for the state once questions of `asked` characters are
    /// in the request: never less than half, so questions too long to hold
    /// to their share (option names are never shortened) still leave a
    /// state to ask them of.
    pub const fn state(self, asked: usize) -> StateBudget {
        let questions = self.questions();
        StateBudget(self.0 - if asked < questions { asked } else { questions })
    }
}

impl Default for RequestBudget {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// How much text a state may carry, in characters: what its questions leave
/// of the [`RequestBudget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateBudget(usize);

impl StateBudget {
    pub const DEFAULT: Self = Self(RequestBudget::DEFAULT.0);

    pub const fn chars(chars: usize) -> Self {
        Self(chars)
    }

    pub const fn get(self) -> usize {
        self.0
    }
}

impl Default for StateBudget {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Which parts of a text decide the answer, when not all of it fits: the
/// sentences kept first wherever they sit, then a run from one end or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keep<'a> {
    first: First<'a>,
    then: Then,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum First<'a> {
    Nothing,
    Questions,
    Naming(&'a [String]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Then {
    Start,
    End,
    Ends,
}

impl Keep<'static> {
    /// The opening: a summary leads with its conclusion.
    pub const START: Self = Self {
        first: First::Nothing,
        then: Then::Start,
    };
    /// The closing lines: a verdict sits at the end.
    pub const END: Self = Self {
        first: First::Nothing,
        then: Then::End,
    };
    /// A request: its question sentences, then its opening and its closing
    /// working inwards, since an ask without a question mark opens or
    /// closes the message.
    pub const REQUEST: Self = Self {
        first: First::Questions,
        then: Then::Ends,
    };
}

impl<'a> Keep<'a> {
    /// The sentences that name one of `terms` before anything else, the
    /// latest first: a conclusion is kept wherever it was written.
    pub const fn naming(self, terms: &'a [String]) -> Self {
        Keep {
            first: First::Naming(terms),
            then: self.then,
        }
    }
}

/// What stands in for text that was left out.
pub const ELISION: &str = "[…]";

/// Fewer lines than this are kept as written: a two-line quote or snippet
/// is usually the point of the message, not noise around it.
const MIN_RUN: usize = 3;

/// The shortest part a long sentence is kept in.
const MIN_PART: usize = 16;

/// How many parts of a long sentence fill the limit: the most that is
/// wasted when the next part does not fit is one of them.
const PARTS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Noise {
    Code,
    Log,
    Quote,
}

impl Noise {
    fn marker(self, lines: usize) -> String {
        match self {
            Noise::Code => format!("[{lines} lines of code]"),
            Noise::Log => format!("[{lines} lines of log]"),
            Noise::Quote => format!("[{lines} quoted lines]"),
        }
    }
}

fn opens_with_fence(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("```") || line.starts_with("~~~")
}

/// Whether `line` opens or closes a fenced block. A line that does both,
/// as chat clients write a one-line snippet, does neither.
fn toggles_fence(line: &str) -> bool {
    line.trim_start().starts_with("~~~") || line.matches("```").count() % 2 == 1
}

/// Which lines sit in a fenced block, fences included. Text before an
/// opening fence on its line stays prose, and a fence that never closes is
/// not one: what follows it may be the question.
fn fenced(lines: &[&str]) -> Vec<bool> {
    let fences: Vec<usize> = (0..lines.len())
        .filter(|&i| toggles_fence(lines[i]))
        .collect();
    let mut inside = vec![false; lines.len()];
    for pair in fences.chunks_exact(2) {
        let opened = pair[0] + usize::from(!opens_with_fence(lines[pair[0]]));
        inside[opened..=pair[1]].fill(true);
    }
    inside
}

fn starts_with_digits(text: &str, digits: usize, then: char) -> Option<&str> {
    let bytes = text.as_bytes();
    (bytes.len() > digits
        && bytes[..digits].iter().all(u8::is_ascii_digit)
        && bytes[digits] == then as u8)
        .then(|| &text[digits + 1..])
}

/// `2026-09-26…` or `14:03:22…`.
fn is_timestamp(token: &str) -> bool {
    let date = starts_with_digits(token, 4, '-').and_then(|rest| starts_with_digits(rest, 2, '-'));
    let time = starts_with_digits(token, 2, ':').and_then(|rest| starts_with_digits(rest, 2, ':'));
    date.or(time).is_some()
}

fn is_level(token: &str) -> bool {
    matches!(
        token,
        "TRACE" | "DEBUG" | "INFO" | "WARN" | "WARNING" | "ERROR" | "FATAL" | "CRITICAL"
    )
}

fn is_log(line: &str) -> bool {
    let stamped = line
        .split_whitespace()
        .take(3)
        .map(|token| token.trim_matches(['[', ']', ':', '|', '<', '>', '(', ')']))
        .any(|token| is_timestamp(token) || is_level(token));
    stamped || (line.starts_with("{\"") && line.ends_with('}'))
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// `src/main.rs:616:12`, `/app/x.js:1`.
fn ends_with_line_number(text: &str) -> bool {
    text.rsplit_once(':')
        .is_some_and(|(_, number)| is_digits(number))
}

fn is_stack_frame(line: &str) -> bool {
    let numbered_frame = || {
        line.split_once(": ").is_some_and(|(index, frame)| {
            is_digits(index)
                && (frame.contains("::")
                    || frame.contains("0x")
                    || (frame.contains('_') && !frame.contains(' ')))
        })
    };
    let at = || {
        line.strip_prefix("at ").is_some_and(|site| {
            site.contains('(')
                || site.contains("::")
                || (!site.contains(' ') && ends_with_line_number(site))
        })
    };
    let go_site = || {
        line.split(".go:")
            .nth(1)
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
    };
    at() || line.starts_with("File \"")
        || line.starts_with("Traceback (most recent call last)")
        || line.starts_with("Caused by:")
        || line.starts_with("goroutine ")
        || (line.starts_with("from ") && line.contains(":in "))
        || (line.starts_with("thread '") && line.contains("panicked at"))
        || (line.starts_with("... ") && line.ends_with(" more"))
        || numbered_frame()
        || go_site()
}

/// A table row is data the question may be about, whatever its cells hold.
fn noise(line: &str) -> Option<Noise> {
    let line = line.trim();
    if line.starts_with('>') || line.starts_with("&gt;") {
        Some(Noise::Quote)
    } else if !line.starts_with('|') && (is_log(line) || is_stack_frame(line)) {
        Some(Noise::Log)
    } else {
        None
    }
}

fn indent(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

/// A line that cannot end a trace on its own: blank, or a bare call such
/// as Go's `main.main()`.
fn bridges(line: &str) -> bool {
    let line = line.trim();
    line.is_empty() || (line.ends_with(')') && !line.contains(char::is_whitespace))
}

/// For each line, whether the nearest line before it (in `order`) that is
/// not a bridge is a log line.
fn after_log(
    classes: &[Option<Noise>],
    lines: &[&str],
    order: impl Iterator<Item = usize>,
) -> Vec<bool> {
    let mut after = vec![false; lines.len()];
    let mut log = false;
    for i in order {
        after[i] = log;
        log = classes[i] == Some(Noise::Log) || (log && classes[i].is_none() && bridges(lines[i]));
    }
    after
}

/// What each line is. A trace is more than its frames: the source line
/// under a frame is indented deeper than it, and blank lines and bare calls
/// sit between frames, so those join the log around them.
fn classes(lines: &[&str]) -> Vec<Option<Noise>> {
    let alone: Vec<Option<Noise>> = lines
        .iter()
        .zip(fenced(lines))
        .map(|(line, fenced)| {
            if fenced {
                Some(Noise::Code)
            } else {
                noise(line)
            }
        })
        .collect();
    let under_frames: Vec<Option<Noise>> = alone
        .iter()
        .zip(lines)
        .scan(None, |frame: &mut Option<usize>, (class, line)| {
            let under = frame.is_some_and(|depth| indent(line) > depth);
            match class {
                Some(Noise::Log) => *frame = Some(indent(line)),
                None if under && !line.trim().is_empty() => return Some(Some(Noise::Log)),
                None if line.trim().is_empty() => {}
                _ => *frame = None,
            }
            Some(*class)
        })
        .collect();
    let before = after_log(&under_frames, lines, 0..lines.len());
    let after = after_log(&under_frames, lines, (0..lines.len()).rev());
    under_frames
        .iter()
        .enumerate()
        .map(|(i, class)| match class {
            None if bridges(lines[i]) && before[i] && after[i] => Some(Noise::Log),
            class => *class,
        })
        .collect()
}

/// `text` with every pasted log, stack trace, fenced code block and long
/// quoted block replaced by a one-line marker such as `[42 lines of log]`.
/// The sentences around them are untouched; runs of blank lines become one.
/// A text that is nothing but such blocks is kept as written: the paste is
/// then the message.
pub fn collapse(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let classed: Vec<(Option<Noise>, &str)> = classes(&lines).into_iter().zip(lines).collect();
    let runs: Vec<Result<Vec<&str>, String>> = classed
        .chunk_by(|a, b| a.0 == b.0)
        .map(|run| {
            let lines = run
                .iter()
                .filter(|(_, line)| !opens_with_fence(line))
                .count();
            match run[0].0 {
                Some(noise) if lines >= MIN_RUN => Err(noise.marker(lines)),
                _ => Ok(run.iter().map(|(_, line)| *line).collect()),
            }
        })
        .collect();
    let says_something = runs
        .iter()
        .flatten()
        .flatten()
        .any(|line| !line.trim().is_empty());
    let mut lines: Vec<String> = if says_something {
        runs.into_iter()
            .flat_map(|run| match run {
                Ok(lines) => lines.into_iter().map(str::to_string).collect(),
                Err(marker) => vec![marker],
            })
            .collect()
    } else {
        classed.iter().map(|(_, line)| line.to_string()).collect()
    };
    lines.dedup_by(|line, previous| line.trim().is_empty() && previous.trim().is_empty());
    lines.join("\n")
}

/// Lowercase, with `-` and `_` read as spaces, so `changes-requested` is
/// named by "Changes requested".
pub(crate) fn plain(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|c| if matches!(c, '-' | '_') { ' ' } else { c })
        .collect()
}

/// Whether `text` names `term` as whole words; both already [`plain`].
pub(crate) fn names(text: &str, term: &str) -> bool {
    !term.is_empty()
        && text.match_indices(term).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + term.len()..].chars().next();
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        })
}

/// A sentence or a part of one, or a line that is not a sentence, and the
/// line it came from.
struct Piece<'a> {
    line: usize,
    text: &'a str,
}

impl Piece<'_> {
    /// Ends in a question mark, whatever closes it after that: `?*`, `?"`,
    /// `?!`, `? 🙏`.
    fn is_question(&self) -> bool {
        self.text
            .chars()
            .rev()
            .take_while(|c| !c.is_alphanumeric())
            .any(|c| matches!(c, '?' | '？' | '؟'))
    }
}

fn longer_than(text: &str, chars: usize) -> bool {
    text.chars().nth(chars).is_some()
}

fn sentences(line: &str) -> impl Iterator<Item = &str> {
    let ends = line
        .char_indices()
        .zip(line.chars().skip(1))
        .filter(|((_, c), next)| {
            matches!(c, '。' | '！' | '？')
                || (matches!(c, '.' | '!' | '?' | '؟' | '۔' | '।') && next.is_whitespace())
        })
        .map(|((i, c), _)| i + c.len_utf8());
    let bounds: Vec<usize> = std::iter::once(0)
        .chain(ends)
        .chain(std::iter::once(line.len()))
        .collect();
    (0..bounds.len() - 1)
        .map(move |i| line[bounds[i]..bounds[i + 1]].trim())
        .filter(|sentence| !sentence.is_empty())
}

/// A mark that belongs to the character before it: a joiner, a variation
/// selector, a combining accent, a skin tone.
fn joins_previous(c: char) -> bool {
    matches!(
        c,
        '\u{200D}'
            | '\u{FE0E}'
            | '\u{FE0F}'
            | '\u{20E3}'
            | '\u{0300}'..='\u{036F}'
            | '\u{3099}'
            | '\u{309A}'
            | '\u{1F3FB}'..='\u{1F3FF}'
    )
}

/// Whether a cut at byte `at` would part a character from a mark joined to
/// it.
fn glued(text: &str, at: usize) -> bool {
    text[at..].chars().next().is_some_and(joins_previous) || text[..at].ends_with('\u{200D}')
}

/// The latest place at or before `at` where `text` can be cut without
/// parting a character from its marks; the start when there is none.
fn cut_before(text: &str, at: usize) -> usize {
    std::iter::successors(Some(at), |&i| {
        text[..i].char_indices().next_back().map(|(j, _)| j)
    })
    .find(|&i| i == 0 || !glued(text, i))
    .unwrap_or(0)
}

/// `sentence` in parts of at most `size` characters, split at whitespace
/// where there is any and between characters where there is none.
fn parts(sentence: &str, size: usize) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = sentence;
    while let Some((limit, next)) = rest.char_indices().nth(size) {
        let at = if next.is_whitespace() {
            limit
        } else {
            rest[..limit]
                .rfind(char::is_whitespace)
                .or_else(|| Some(cut_before(rest, limit)).filter(|at| *at > 0))
                .unwrap_or(limit)
        };
        parts.push(rest[..at].trim_end());
        rest = rest[at..].trim_start();
    }
    parts.push(rest);
    parts
}

/// The sentences of `text`. One longer than half the limit could crowd out
/// everything else or be dropped whole, so it is kept in parts.
fn pieces(text: &str, limit: usize) -> Vec<Piece<'_>> {
    let part = (limit / PARTS).max(MIN_PART);
    text.lines()
        .enumerate()
        .flat_map(|(line, content)| {
            sentences(content)
                .flat_map(move |sentence| {
                    if longer_than(sentence, limit / 2) {
                        parts(sentence, part)
                    } else {
                        vec![sentence]
                    }
                })
                .filter(|part| !part.is_empty())
                .map(move |text| Piece { line, text })
        })
        .collect()
}

impl First<'_> {
    /// The pieces to keep before any other, most worth keeping first.
    fn order(self, pieces: &[Piece<'_>]) -> Vec<usize> {
        match self {
            First::Nothing => Vec::new(),
            First::Questions => (0..pieces.len())
                .filter(|&i| pieces[i].is_question())
                .collect(),
            First::Naming(terms) => {
                let terms: Vec<String> = terms.iter().map(|term| plain(term.trim())).collect();
                (0..pieces.len())
                    .rev()
                    .filter(|&i| {
                        let text = plain(pieces[i].text);
                        terms.iter().any(|term| names(&text, term))
                    })
                    .collect()
            }
        }
    }
}

/// The chosen pieces in their original order, an [`ELISION`] wherever some
/// were left out.
fn render(pieces: &[Piece<'_>], chosen: &BTreeSet<usize>) -> String {
    let mut out = String::new();
    let mut previous: Option<usize> = None;
    for &i in chosen {
        match previous {
            None if i > 0 => out.extend([ELISION, " "]),
            None => {}
            Some(p) if i > p + 1 => out.extend([" ", ELISION, " "]),
            Some(p) if pieces[p].line != pieces[i].line => out.push('\n'),
            Some(_) => out.push(' '),
        }
        out.push_str(pieces[i].text);
        previous = Some(i);
    }
    if previous.is_some_and(|p| p + 1 < pieces.len()) {
        out.extend([" ", ELISION]);
    }
    out
}

/// The pieces kept so far, never more than fit the limit.
struct Kept<'a> {
    pieces: &'a [Piece<'a>],
    limit: usize,
    chosen: BTreeSet<usize>,
}

impl Kept<'_> {
    /// Keep piece `i` if it still fits; whether it is kept.
    fn keep(&mut self, i: usize) -> bool {
        self.keep_within(i, self.limit)
    }

    /// Keep piece `i` if what is kept then stays within `chars`.
    fn keep_within(&mut self, i: usize, chars: usize) -> bool {
        if !self.chosen.insert(i) {
            return true;
        }
        let fits = !longer_than(&render(self.pieces, &self.chosen), chars);
        if !fits {
            self.chosen.remove(&i);
        }
        fits
    }

    fn chars(&self) -> usize {
        render(self.pieces, &self.chosen).chars().count()
    }

    /// Keep pieces in `order` up to the first that does not fit.
    fn run(&mut self, order: impl IntoIterator<Item = usize>) {
        for i in order {
            if !self.keep(i) {
                break;
            }
        }
    }

    /// Keep the opening and the closing, a piece from each in turn, each
    /// side up to the first that does not fit.
    fn ends(&mut self) {
        let last = self.pieces.len().checked_sub(1);
        let (mut head, mut tail) = (last.map(|_| 0), last);
        while head.is_some() || tail.is_some() {
            head = head
                .filter(|&i| self.keep(i))
                .map(|i| i + 1)
                .filter(|next| Some(*next) <= last);
            tail = tail
                .filter(|&i| self.keep(i))
                .and_then(|i| i.checked_sub(1));
        }
    }
}

/// A text nothing of which fits as whole pieces, cut at a word where one is
/// near and never inside a character.
fn cut(text: &str, limit: usize, then: Then) -> String {
    let Some(room) = limit.checked_sub(1).filter(|room| *room > 0) else {
        return "…".chars().take(limit).collect();
    };
    match then {
        Then::End => {
            let start = text
                .char_indices()
                .rev()
                .nth(room - 1)
                .map_or(0, |(i, _)| i);
            let start = (start..text.len())
                .find(|&i| text.is_char_boundary(i) && !glued(text, i))
                .unwrap_or(text.len());
            let tail = &text[start..];
            let tail = tail
                .find(char::is_whitespace)
                .filter(|i| *i < tail.len() / 2)
                .map_or(tail, |i| tail[i..].trim_start());
            format!("…{tail}")
        }
        Then::Start | Then::Ends => {
            let end = text.char_indices().nth(room).map_or(text.len(), |(i, _)| i);
            let head = &text[..cut_before(text, end)];
            let head = head
                .rfind(char::is_whitespace)
                .filter(|i| *i > head.len() / 2)
                .map_or(head, |i| head[..i].trim_end());
            format!("{head}…")
        }
    }
}

/// `text` in at most `limit` characters: whole when it fits, otherwise the
/// sentences and lines `keep` values most, in their original order, with
/// [`ELISION`] marking what was left out. Never empty for a text that is
/// not, unless the limit is zero.
///
/// The very opening or closing is kept before anything else, and the
/// sentences kept first take at most half the limit after it: a text that
/// names its terms, or asks, over and over must not crowd out the line it
/// ends on.
pub fn fit(text: &str, limit: usize, keep: Keep<'_>) -> String {
    let text = text.trim();
    if !longer_than(text, limit) {
        return text.to_string();
    }
    let pieces = pieces(text, limit);
    let mut kept = Kept {
        pieces: &pieces,
        limit,
        chosen: BTreeSet::new(),
    };
    let last = pieces.len() - 1;
    let edges = match keep.then {
        Then::Start => vec![0],
        Then::End => vec![last],
        Then::Ends => vec![0, last],
    };
    for edge in edges {
        kept.keep(edge);
    }
    let first = (kept.chars() + limit / 2).min(limit);
    for i in keep.first.order(&pieces) {
        kept.keep_within(i, first);
    }
    match keep.then {
        Then::Start => kept.run(0..pieces.len()),
        Then::End => kept.run((0..pieces.len()).rev()),
        Then::Ends => kept.ends(),
    }
    if kept.chosen.is_empty() {
        cut(text, limit, keep.then)
    } else {
        render(&pieces, &kept.chosen)
    }
}

/// A share of `total` for each need: nothing gets more than it needs, and
/// what the small ones leave over goes to the large.
pub(crate) fn allot(needs: &[usize], total: usize) -> Vec<usize> {
    let mut smallest_first: Vec<usize> = (0..needs.len()).collect();
    smallest_first.sort_by_key(|&i| needs[i]);
    let mut grants = vec![0; needs.len()];
    let mut left = total;
    for (rank, &i) in smallest_first.iter().enumerate() {
        grants[i] = needs[i].min(left / (needs.len() - rank));
        left -= grants[i];
    }
    grants
}

/// Several texts sharing one limit, each collapsed and then fitted to its
/// share.
pub(crate) fn fit_all(texts: &[(&str, Keep<'_>)], limit: usize) -> Vec<String> {
    let collapsed: Vec<String> = texts.iter().map(|(text, _)| collapse(text)).collect();
    let needs: Vec<usize> = collapsed.iter().map(|t| t.trim().chars().count()).collect();
    collapsed
        .iter()
        .zip(texts)
        .zip(allot(&needs, limit))
        .map(|((text, (_, keep)), share)| fit(text, share, *keep))
        .collect()
}

fn strings<'a>(value: &'a Value, found: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => found.push(text),
        Value::Array(items) => items.iter().for_each(|item| strings(item, found)),
        Value::Object(fields) => fields.values().for_each(|field| strings(field, found)),
        _ => {}
    }
}

fn replace_strings(value: &Value, with: &mut impl Iterator<Item = String>) -> Value {
    match value {
        Value::String(text) => Value::String(with.next().unwrap_or_else(|| text.clone())),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| replace_strings(item, with))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, field)| (key.clone(), replace_strings(field, with)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The characters of text in `value`: its strings, wherever they sit.
pub fn chars(value: &Value) -> usize {
    let mut found = Vec::new();
    strings(value, &mut found);
    found.iter().map(|text| text.chars().count()).sum()
}

/// Values someone else wrote, held to `limit` characters between them:
/// every string is collapsed and fitted to its share from its opening.
/// Keys, numbers and structure are never touched.
pub(crate) fn fit_values(values: &[&Value], limit: usize) -> Vec<Value> {
    let mut found = Vec::new();
    values.iter().for_each(|value| strings(value, &mut found));
    let texts: Vec<(&str, Keep<'_>)> = found.into_iter().map(|t| (t, Keep::START)).collect();
    let mut fitted = fit_all(&texts, limit).into_iter();
    values
        .iter()
        .map(|value| replace_strings(value, &mut fitted))
        .collect()
}

/// A state someone else wrote, held to the budget: every string in it is
/// collapsed, and when together they exceed the budget each is fitted to its
/// share from its opening. Keys, numbers and structure are never touched.
pub fn fit_value(value: &Value, budget: StateBudget) -> Value {
    fit_values(&[value], budget.get())
        .pop()
        .unwrap_or_else(|| value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn log_lines(n: usize) -> String {
        (0..n)
            .map(|i| format!("2026-09-26T10:00:{i:02}Z INFO worker: tick {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn logs_traces_code_and_long_quotes_collapse_to_markers_and_prose_stays() {
        let message = format!(
            "The nightly import is failing, can you look?\n{}\n\
             java.lang.IllegalStateException: no rows\n\
             \tat com.example.Import.run(Import.java:42)\n\
             \tat com.example.Main.main(Main.java:7)\n\
             Caused by: java.io.IOException: closed\n\
             ```sql\nSELECT 1;\nSELECT 2;\nSELECT 3;\n```\n\
             > first quoted\n> second quoted\n> third quoted\n\
             Is it the schema change?",
            log_lines(42)
        );
        let collapsed = collapse(&message);
        assert_eq!(
            collapsed,
            "The nightly import is failing, can you look?\n\
             [42 lines of log]\n\
             java.lang.IllegalStateException: no rows\n\
             [3 lines of log]\n\
             [3 lines of code]\n\
             [3 quoted lines]\n\
             Is it the schema change?"
        );
        assert_eq!(
            collapse(&collapsed),
            collapsed,
            "collapsing twice changes nothing"
        );
    }

    #[test]
    fn short_snippets_stay_and_blank_runs_become_one() {
        let short = "Use this:\n```\nls -la\n```\n> as you said\nthanks";
        assert_eq!(collapse(short), short);
        assert_eq!(collapse(""), "");
        assert_eq!(collapse(" \n\t\n"), " ");
        assert_eq!(collapse("one\n\n \n\ntwo"), "one\n\ntwo");
        assert_eq!(collapse("one\r\ntwo\r\n"), "one\ntwo");
    }

    #[test]
    fn a_fence_is_only_a_block_that_opens_and_closes() {
        assert_eq!(
            collapse("See:\n```\na\nb\nc\nd"),
            "See:\n```\na\nb\nc\nd",
            "a fence that never closes is not one"
        );
        assert_eq!(
            collapse("```one line```\nfirst\nsecond\nthird\nWhy?"),
            "```one line```\nfirst\nsecond\nthird\nWhy?",
            "a one-line snippet opens nothing"
        );
        assert_eq!(
            collapse("I get this: ```Traceback\na\nb\nc```\nWhy?"),
            "I get this: ```Traceback\n[3 lines of code]\nWhy?",
            "words before an opening fence stay"
        );
        assert_eq!(
            collapse("```\na\nb\nc\n```\nbetween\n~~~\nd\ne\nf\n~~~\nWhy?"),
            "[3 lines of code]\nbetween\n[3 lines of code]\nWhy?"
        );
    }

    #[test]
    fn a_trace_is_its_frames_and_what_sits_under_and_between_them() {
        let python = "Traceback (most recent call last):\n  File \"a.py\", line 3, in <module>\n    main()\n  File \"a.py\", line 1, in main\n    raise ValueError(\"x\")\nValueError: x";
        assert_eq!(collapse(python), "[5 lines of log]\nValueError: x");
        let go = "panic: boom\n\ngoroutine 1 [running]:\nmain.load(...)\n\t/app/load.go:17 +0x1d\nmain.main()\n\t/app/main.go:9 +0x17\nexit status 2";
        assert_eq!(
            collapse(go),
            "panic: boom\n\n[5 lines of log]\nexit status 2"
        );
        let wrapped = "2026-09-26T10:00:00Z ERROR db: query failed\n    caused by: connection reset\n    while running: SELECT 1\nWhat now?";
        assert_eq!(collapse(wrapped), "[3 lines of log]\nWhat now?");
        let jsonl = "{\"level\":\"info\",\"msg\":\"a\"}\n{\"level\":\"info\",\"msg\":\"b\"}\n{\"level\":\"warn\",\"msg\":\"c\"}";
        assert_eq!(
            collapse(&format!("Look:\n{jsonl}")),
            "Look:\n[3 lines of log]"
        );
    }

    #[test]
    fn ordinary_prose_tables_and_lists_are_never_mistaken_for_a_log() {
        for prose in [
            "1. First do this.\n2. Then that.\n3. Then the other.\n\
             At 14:00 we deploy.\nat the end we check (again).\nInfo is on the wiki.",
            "1: yes\n2: no\n3: maybe\n4: ask again",
            "| 2026-09-01 | 3 |\n| 2026-09-02 | 4 |\n| 2026-09-03 | 5 |",
            "- check the logs\n  - then the dashboard\n  - then page someone\n- write it up",
            "ERROR budgets matter.\nWhat is ours?\nERROR rates are up.",
        ] {
            assert_eq!(collapse(prose), prose);
        }
    }

    #[test]
    fn a_text_that_is_all_paste_is_kept_as_written() {
        let log = log_lines(5);
        assert_eq!(collapse(&log), log);
        assert_eq!(collapse(&format!("\n\n{log}\n\n\n")), format!("\n{log}\n"));
        assert_eq!(collapse(&format!("See\n{log}")), "See\n[5 lines of log]");
    }

    #[test]
    fn text_within_the_limit_is_returned_whole() {
        assert_eq!(
            fit("  short and whole.  ", 100, Keep::START),
            "short and whole."
        );
        assert_eq!(fit(" \n ", 100, Keep::REQUEST), "");
    }

    #[test]
    fn the_questions_survive_and_what_is_dropped_is_marked() {
        let filler = "This sentence is background that does not decide anything. ".repeat(20);
        let text = format!("Hello team. {filler}Why did the invoice job fail? {filler}Thanks!");
        let fitted = fit(&text, 200, Keep::REQUEST);
        assert!(fitted.chars().count() <= 200, "{fitted}");
        assert!(fitted.contains("Why did the invoice job fail?"), "{fitted}");
        assert!(fitted.starts_with("Hello team."), "{fitted}");
        assert!(fitted.ends_with("Thanks!"), "{fitted}");
        assert!(fitted.contains(ELISION), "{fitted}");
    }

    #[test]
    fn a_question_is_one_whatever_closes_it() {
        let asks = |text: &str| Piece { line: 0, text }.is_question();
        for question in [
            "Is it down?",
            "*Is it down?*",
            "\"Is it down?\"",
            "Is it down?!",
            "Is it down?🙏",
            "(is it down?)",
            "落ちていますか？",
            "هل هو معطل؟",
        ] {
            assert!(asks(question), "{question}");
        }
        for statement in ["It is down.", "See https://x.example/a?b=1", "Down? No"] {
            assert!(!asks(statement), "{statement}");
        }
    }

    #[test]
    fn a_question_too_long_to_keep_whole_keeps_its_end_and_is_never_dropped() {
        let ramble = "and then the page reloaded and the cart was empty again ".repeat(40);
        let text = format!("Hi there. So I clicked pay {ramble}so what should I do now? Thanks.");
        let fitted = fit(&text, 300, Keep::REQUEST);
        assert!(fitted.chars().count() <= 300, "{fitted}");
        assert!(fitted.contains("so what should I do now?"), "{fitted}");
        assert!(fitted.starts_with("Hi there."), "{fitted}");
    }

    #[test]
    fn named_sentences_are_kept_first_and_as_whole_words() {
        let filler = "Nothing of note in this file either. ".repeat(40);
        let text = format!("{filler}The change is not-ready as it stands. {filler}Bye.");
        let terms = ["not_ready".to_string()];
        let end = fit(&text, 200, Keep::END.naming(&terms));
        assert!(
            end.contains("The change is not-ready as it stands."),
            "{end}"
        );
        assert!(end.ends_with("Bye.") && end.chars().count() <= 200);
        let start = fit(&text, 200, Keep::START.naming(&terms));
        assert!(
            start.contains("The change is not-ready as it stands."),
            "{start}"
        );
        assert!(start.starts_with("Nothing of note"));
        assert!(names("it is done", "done") && names("done.", "done"));
        assert!(!names("it is undone", "done") && !names("abandoned", "done"));
        assert!(!names("anything", ""));
        assert_eq!(plain("Changes-Requested_NOW"), "changes requested now");
    }

    #[test]
    fn sentences_kept_first_never_crowd_out_the_closing_or_the_opening() {
        let passing = "Last round this was not approved and changes were requested. ".repeat(40);
        let terms = ["approved".to_string(), "changes-requested".to_string()];
        let closing = "So it is not ready to merge yet.";
        let end = fit(
            &format!("{passing}Some notes. Some more notes. {closing}"),
            300,
            Keep::END.naming(&terms),
        );
        assert!(end.chars().count() <= 300, "{end}");
        assert!(
            end.ends_with(&format!("Some notes. Some more notes. {closing}")),
            "{end}"
        );
        assert!(end.contains("changes were requested"), "{end}");

        let opening = "Summary: nothing blocks the merge.";
        let start = fit(
            &format!("{opening} Details follow. {passing}"),
            300,
            Keep::START.naming(&terms),
        );
        assert!(
            start.starts_with(&format!("{opening} Details follow.")),
            "{start}"
        );

        let asking = "And what about this other thing? ".repeat(60);
        let request = fit(
            &format!("Our importer is down. {asking}Please page whoever is on call."),
            300,
            Keep::REQUEST,
        );
        assert!(request.starts_with("Our importer is down."), "{request}");
        assert!(
            request.ends_with("Please page whoever is on call."),
            "{request}"
        );
        assert!(request.contains("And what about this other thing?"));
    }

    #[test]
    fn the_end_or_the_start_is_kept_as_one_unbroken_run() {
        let text = (1..=40)
            .map(|i| format!("Line number {i} of the report."))
            .collect::<Vec<_>>()
            .join("\n");
        let end = fit(&text, 120, Keep::END);
        assert!(end.chars().count() <= 120);
        assert!(end.starts_with(ELISION) && end.ends_with("Line number 40 of the report."));
        assert!(end.contains("Line number 39 of the report.\nLine number 40"));
        let start = fit(&text, 120, Keep::START);
        assert!(start.chars().count() <= 120);
        assert!(start.starts_with("Line number 1 of the report.\nLine number 2"));
        assert!(start.ends_with(ELISION));
        let ends = fit(&text, 120, Keep::REQUEST);
        assert!(ends.chars().count() <= 120);
        assert!(ends.starts_with("Line number 1 of the report."), "{ends}");
        assert!(ends.ends_with("Line number 40 of the report."), "{ends}");
    }

    #[test]
    fn an_overlong_sentence_is_kept_in_parts_split_at_words() {
        let words = "détaillé ".repeat(100);
        let head = fit(&words, 50, Keep::START);
        assert!(head.chars().count() <= 50, "{head}");
        assert!(head.starts_with("détaillé détaillé") && head.ends_with(ELISION));
        assert!(
            head.split(' ').all(|w| w == "détaillé" || w == ELISION),
            "{head}"
        );
        let tail = fit(&words, 50, Keep::END);
        assert!(tail.chars().count() <= 50, "{tail}");
        assert!(tail.starts_with(ELISION) && tail.ends_with("détaillé détaillé"));
    }

    #[test]
    fn text_with_no_place_to_split_is_cut_between_characters() {
        let unbroken = "é".repeat(500);
        assert_eq!(
            fit(&unbroken, 10, Keep::START),
            format!("{}…", "é".repeat(9))
        );
        assert_eq!(
            fit(&"é".repeat(512), 10, Keep::END),
            format!("…{}", "é".repeat(9))
        );
        assert_eq!(fit(&unbroken, 1, Keep::START), "…");
        assert_eq!(fit(&unbroken, 0, Keep::START), "");
        let long = fit(&unbroken, 400, Keep::START);
        assert!((350..=400).contains(&long.chars().count()), "{long}");
        assert!(long.ends_with(ELISION));
    }

    #[test]
    fn a_cut_never_parts_an_emoji_from_its_joiners_or_a_letter_from_its_accent() {
        let family = "👨\u{200D}👩\u{200D}👧\u{200D}👦";
        let accented = "e\u{0301}";
        let thumbs = "👍\u{1F3FD}";
        for cluster in [family, accented, thumbs] {
            let text = cluster.repeat(300);
            for limit in 2..40 {
                for keep in [Keep::START, Keep::END, Keep::REQUEST] {
                    let fitted = fit(&text, limit, keep);
                    assert!(fitted.chars().count() <= limit);
                    let body = fitted.replace(ELISION, "").replace(['…', ' '], "");
                    assert_eq!(
                        body.replace(cluster, ""),
                        "",
                        "{limit} {keep:?}: a cluster was parted in {fitted:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn multibyte_text_never_exceeds_the_limit_and_is_never_empty() {
        let texts = [
            "日本語の文章です。 これは質問ですか? 🎉🎉🎉 emoji 行。 ".repeat(60),
            "한국어 문장입니다 그리고 계속됩니다 ".repeat(80),
            "🎉".repeat(900),
            format!("{}\n", "| a | b |").repeat(300),
        ];
        for text in &texts {
            for limit in [0, 1, 2, 3, 7, 40, 199, 1_500] {
                for keep in [Keep::START, Keep::END, Keep::REQUEST] {
                    let fitted = fit(text, limit, keep);
                    assert!(
                        fitted.chars().count() <= limit,
                        "{limit} {keep:?}: {fitted}"
                    );
                    assert_eq!(fitted.is_empty(), limit == 0, "{limit} {keep:?}");
                    assert_eq!(fit(text, limit, keep), fitted, "stable");
                }
            }
        }
    }

    #[test]
    fn a_megabyte_on_one_line_is_fitted_quickly() {
        let line = "word ".repeat(250_000);
        let started = std::time::Instant::now();
        let fitted = fit(&collapse(&line), 1_500, Keep::REQUEST);
        assert!(fitted.chars().count() <= 1_500);
        let lines = "2026-09-26T10:00:00Z INFO tick\n\n".repeat(60_000);
        assert_eq!(
            collapse(&format!("Why?\n{lines}")),
            "Why?\n[119999 lines of log]\n"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn shares_go_to_what_needs_them() {
        assert_eq!(allot(&[10, 20, 30], 100), vec![10, 20, 30]);
        assert_eq!(allot(&[10, 500, 500], 100), vec![10, 45, 45]);
        assert_eq!(allot(&[500, 10, 40], 100), vec![50, 10, 40]);
        assert_eq!(allot(&[], 100), Vec::<usize>::new());
        assert_eq!(allot(&[5, 5], 0), vec![0, 0]);
    }

    #[test]
    fn a_supplied_state_keeps_its_structure_and_fits_its_strings() {
        let state = json!({
            "title": "Checkout fails",
            "priority": 2,
            "tags": ["billing", "p1"],
            "body": format!("Customers cannot pay. {}", "More detail follows here. ".repeat(200)),
            "log": format!("Seen in production:\n{}", log_lines(30)),
        });
        let fitted = fit_value(&state, StateBudget::chars(300));
        assert_eq!(fitted["title"], "Checkout fails");
        assert_eq!(fitted["priority"], 2);
        assert_eq!(fitted["tags"], json!(["billing", "p1"]));
        assert_eq!(fitted["log"], "Seen in production:\n[30 lines of log]");
        let body = fitted["body"].as_str().unwrap();
        assert!(body.starts_with("Customers cannot pay.") && body.ends_with(ELISION));
        let mut kept = Vec::new();
        strings(&fitted, &mut kept);
        assert!(kept.iter().map(|t| t.chars().count()).sum::<usize>() <= 300);
        assert_eq!(fit_value(&state, StateBudget::chars(300)), fitted, "stable");

        let small = json!({"a": "one", "n": [1, true, null]});
        assert_eq!(fit_value(&small, StateBudget::DEFAULT), small);
    }

    #[test]
    fn a_request_budget_has_a_floor_and_leaves_the_state_what_the_questions_do_not_take() {
        assert_eq!(RequestBudget::chars(5).get(), RequestBudget::MIN_CHARS);
        assert_eq!(RequestBudget::chars(4_000).get(), 4_000);
        assert_eq!(RequestBudget::default(), RequestBudget::DEFAULT);
        let budget = RequestBudget::chars(1_000);
        assert_eq!(budget.questions(), 500);
        assert_eq!(budget.state(300), StateBudget::chars(700));
        assert_eq!(budget.state(5_000), StateBudget::chars(500));
    }

    #[test]
    fn the_text_of_a_value_is_its_strings_and_nothing_else() {
        let value = json!({"a": "four", "n": [1, "é", null], "deep": {"k": "ab"}});
        assert_eq!(chars(&value), 7);
        assert_eq!(chars(&json!(12)), 0);
    }

    #[test]
    fn several_values_share_one_limit_in_order() {
        let long = "A sentence of padding. ".repeat(40);
        let (a, b) = (json!({"x": long}), json!(["short", long]));
        let fitted = fit_values(&[&a, &b], 300);
        assert_eq!(fitted[1][0], "short");
        assert!(fitted.iter().map(chars).sum::<usize>() <= 300);
        assert!(fitted[0]["x"].as_str().unwrap().starts_with("A sentence"));
    }
}

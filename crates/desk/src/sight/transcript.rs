//! A session's own record, read backwards from its end by byte offset: a
//! page of its newest exchanges, a search through them, or one call's whole
//! output. Each exchange is folded by session memory's [`fold`], redacted
//! before anything of it is cut. Pure over a record's byte ranges.

use std::collections::VecDeque;
use std::io;
use std::time::Instant;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use crate::ledger;
use crate::memory::harvest::CodexParts;
use crate::memory::record::{elide_middle, fold, Exchange, Part};
use crate::memory::Redactor;
use crate::model::Engine;

/// How much of a record is read at a time.
pub const CHUNK: u64 = 1 << 20;
/// The most characters of rendered exchanges one page holds.
pub const PAGE_CHARS: usize = 12_000;
/// The most of a record one search or step lookup reads.
pub const SCAN_BYTES: u64 = 32 << 20;
/// The most hits one search gives.
const HITS_MAX: usize = 20;
/// How much of an exchange is shown on each side of a hit.
const SNIPPET: usize = 150;
/// The most of one call's output a step lookup gives.
pub const STEP_MAX: usize = 12_000;

/// A record's bytes, read by range.
pub trait Ranges {
    fn size(&self) -> u64;
    fn read_at(&self, at: u64, len: u64) -> io::Result<Vec<u8>>;
}

impl Ranges for [u8] {
    fn size(&self) -> u64 {
        self.len() as u64
    }

    fn read_at(&self, at: u64, len: u64) -> io::Result<Vec<u8>> {
        let start = usize::try_from(at).unwrap_or(usize::MAX).min(self.len());
        let end = usize::try_from(at.saturating_add(len))
            .unwrap_or(usize::MAX)
            .min(self.len());
        Ok(self[start..end].to_vec())
    }
}

/// How far a scan may read: bytes, and until when.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub bytes: u64,
    pub until: Option<Instant>,
}

impl Budget {
    /// No more than a page needs.
    pub const PAGE: Self = Self {
        bytes: u64::MAX,
        until: None,
    };
}

/// One exchange's span of a record, and its parts in order.
struct Span {
    start: u64,
    parts: Vec<Part>,
}

/// A record's exchanges, newest first, each from the line that opens it
/// (a prompt, or an engine's summary of what came before) to the next.
struct Spans<'a, R: ?Sized> {
    r: &'a R,
    engine: Engine,
    pos: u64,
    carry: Vec<u8>,
    lines: VecDeque<(u64, Vec<u8>)>,
    parts: VecDeque<Part>,
    read: u64,
    budget: Budget,
    /// Ran out of budget before the record's start.
    cut: bool,
}

impl<'a, R: Ranges + ?Sized> Spans<'a, R> {
    fn new(r: &'a R, engine: Engine, end: u64, budget: Budget) -> Self {
        Self {
            r,
            engine,
            pos: end.min(r.size()),
            carry: Vec::new(),
            lines: VecDeque::new(),
            parts: VecDeque::new(),
            read: 0,
            budget,
            cut: false,
        }
    }

    fn spent(&self) -> bool {
        self.read >= self.budget.bytes || self.budget.until.is_some_and(|u| Instant::now() >= u)
    }

    /// The record's whole lines, newest first, with their offsets.
    fn line(&mut self) -> io::Result<Option<(u64, Vec<u8>)>> {
        while self.lines.is_empty() {
            if self.pos == 0 {
                return Ok((!self.carry.is_empty()).then(|| (0, std::mem::take(&mut self.carry))));
            }
            if self.spent() {
                self.cut = true;
                return Ok(None);
            }
            let start = self.pos.saturating_sub(CHUNK);
            let mut buf = self.r.read_at(start, self.pos - start)?;
            self.read += self.pos - start;
            buf.append(&mut self.carry);
            let mut pieces = buf.split(|b| *b == b'\n');
            let first = pieces.next().unwrap_or_default().to_vec();
            let mut at = start + first.len() as u64 + 1;
            for piece in pieces {
                if !piece.is_empty() {
                    self.lines.push_back((at, piece.to_vec()));
                }
                at += piece.len() as u64 + 1;
            }
            match start {
                0 if !first.is_empty() => self.lines.push_front((0, first)),
                _ => self.carry = first,
            }
            self.pos = start;
        }
        Ok(self.lines.pop_back())
    }

    fn parts_of(&self, line: &[u8]) -> Vec<Part> {
        match self.engine {
            Engine::Claude => ledger::claude_read(line, false, true).parts,
            Engine::Codex => ledger::codex_read(line, None, Some(&mut CodexParts::default())).parts,
            Engine::Gemini
            | Engine::Copilot
            | Engine::OpenCode
            | Engine::Cursor
            | Engine::Devin
            | Engine::Custom => Vec::new(),
        }
    }

    fn next_span(&mut self) -> io::Result<Option<Span>> {
        while let Some((at, line)) = self.line()? {
            let parts = self.parts_of(&line);
            let opens = parts
                .iter()
                .any(|p| matches!(p, Part::Ask { .. } | Part::Compacted { .. }));
            parts
                .into_iter()
                .rev()
                .for_each(|p| self.parts.push_front(p));
            if opens {
                return Ok(Some(Span {
                    start: at,
                    parts: std::mem::take(&mut self.parts).into(),
                }));
            }
        }
        Ok(None)
    }
}

/// An exchange folded from its parts, redacted.
fn folded(parts: Vec<Part>, redactor: &dyn Redactor) -> Option<Exchange> {
    let (open, closed) = fold(None, parts, redactor);
    closed.into_iter().next().or(open)
}

/// How much of an exchange a page shows.
#[derive(Debug, Clone, Copy)]
struct Shown {
    ask: usize,
    answer: usize,
    steps: usize,
    step: usize,
    files: usize,
    errors: usize,
}

/// From the most an exchange shows to the least, tried in turn until it
/// fits.
const SHOWN: [Shown; 3] = [
    Shown {
        ask: 2000,
        answer: 3000,
        steps: 30,
        step: usize::MAX,
        files: 20,
        errors: 10,
    },
    Shown {
        ask: 600,
        answer: 1000,
        steps: 10,
        step: 200,
        files: 5,
        errors: 3,
    },
    Shown {
        ask: 200,
        answer: 300,
        steps: 3,
        step: 80,
        files: 0,
        errors: 1,
    },
];

/// An exchange as a page shows it.
fn render(x: &Exchange, s: Shown) -> Value {
    let earlier = x.steps.len().saturating_sub(s.steps);
    let steps: Vec<Value> = x.steps[earlier..]
        .iter()
        .map(|st| {
            json!({
                "id": st.id,
                "tool": st.tool,
                "input": elide_middle(&st.input, s.step),
                "result": elide_middle(&st.result, s.step),
                "failed": st.failed,
            })
        })
        .collect();
    let mut v = json!({
        "at": x.at,
        "ended": x.ended,
        "asker": x.asker,
        "ask": elide_middle(&x.ask, s.ask),
        "answer": elide_middle(&x.answer, s.answer),
        "steps": steps,
        "files": x.files.iter().take(s.files).collect::<Vec<_>>(),
        "errors": x.errors.iter().take(s.errors).collect::<Vec<_>>(),
        "compaction": x.compaction,
        "redacted": x.redacted,
        "flagged": x.flagged,
    });
    if earlier > 0 {
        v["earlier_steps"] = json!(earlier);
    }
    v
}

fn chars_of(v: &Value) -> usize {
    v.to_string().chars().count()
}

/// A page of a record's newest exchanges.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Page {
    pub exchanges: Vec<Value>,
    /// Where the page before this one ends; none at the record's start.
    pub before: Option<u64>,
}

/// The exchanges that end at `end`, newest first, as many as fit
/// [`PAGE_CHARS`]; always at least one, cut to fit.
pub fn page<R: Ranges + ?Sized>(
    r: &R,
    engine: Engine,
    end: u64,
    redactor: &dyn Redactor,
) -> io::Result<Page> {
    let mut spans = Spans::new(r, engine, end, Budget::PAGE);
    let mut exchanges = Vec::new();
    let mut used = 0;
    let mut before = None;
    while let Some(span) = spans.next_span()? {
        let Some(x) = folded(span.parts, redactor) else {
            before = Some(span.start);
            continue;
        };
        let first = exchanges.is_empty();
        let shown = match first {
            true => SHOWN
                .iter()
                .map(|s| render(&x, *s))
                .find(|v| chars_of(v) <= PAGE_CHARS)
                .unwrap_or_else(|| render(&x, SHOWN[SHOWN.len() - 1])),
            false => render(&x, SHOWN[0]),
        };
        let size = chars_of(&shown);
        if !first && used + size > PAGE_CHARS {
            break;
        }
        used += size;
        exchanges.push(shown);
        before = Some(span.start);
    }
    Ok(Page {
        exchanges,
        before: before.filter(|b| *b > 0),
    })
}

/// Where a search found its words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hit {
    pub exchange_at: DateTime<Utc>,
    #[serde(rename = "where")]
    pub place: String,
    pub snippet: String,
}

/// What a search found, newest first, and where to search on from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Found {
    pub hits: Vec<Hit>,
    pub before: Option<u64>,
}

/// `text` around the first place `lower` holds `query` (lowercased).
fn snippet(text: &str, query: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let at = lower.find(query)?;
    let chars: Vec<char> = text.chars().collect();
    let first = lower[..at].chars().count().min(chars.len());
    let from = first.saturating_sub(SNIPPET);
    let to = (first + query.chars().count() + SNIPPET).min(chars.len());
    Some(chars[from..to].iter().collect())
}

/// Each place in `x` that holds `query`, as hits.
fn hits_in(x: &Exchange, query: &str) -> Vec<Hit> {
    let steps = x.steps.iter().flat_map(|s| {
        [&s.input, &s.result]
            .into_iter()
            .map(move |t| (format!("step {}", s.tool), t.as_str()))
    });
    [
        ("ask".to_string(), x.ask.as_str()),
        ("answer".into(), x.answer.as_str()),
    ]
    .into_iter()
    .chain(steps)
    .chain(x.errors.iter().map(|e| ("error".to_string(), e.as_str())))
    .filter_map(|(place, text)| {
        snippet(text, query).map(|snippet| Hit {
            exchange_at: x.at,
            place,
            snippet,
        })
    })
    .collect()
}

/// The places in the exchanges that end at `end` holding `query` in any
/// case, newest first: searched in their redacted text, so a secret is
/// never found by its own words. Stops at [`HITS_MAX`] or `budget`, saying
/// where to go on from.
pub fn search<R: Ranges + ?Sized>(
    r: &R,
    engine: Engine,
    end: u64,
    redactor: &dyn Redactor,
    query: &str,
    budget: Budget,
) -> io::Result<Found> {
    let query = query.trim().to_lowercase();
    let mut spans = Spans::new(r, engine, end, budget);
    let mut hits = Vec::new();
    let mut before = None;
    while hits.len() < HITS_MAX {
        let Some(span) = spans.next_span()? else {
            break;
        };
        if let Some(x) = folded(span.parts, redactor) {
            hits.extend(hits_in(&x, &query));
        }
        before = Some(span.start);
    }
    hits.truncate(HITS_MAX);
    let more = hits.len() >= HITS_MAX || spans.cut;
    Ok(Found {
        hits,
        before: before.filter(|b| *b > 0 && more),
    })
}

/// One call, its output whole as far as [`STEP_MAX`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Call {
    pub id: String,
    pub tool: String,
    pub input: String,
    pub result: Option<String>,
    pub failed: bool,
}

/// The call `id` among the exchanges that end at `end`, redacted: none when
/// it isn't within `budget`.
pub fn step<R: Ranges + ?Sized>(
    r: &R,
    engine: Engine,
    end: u64,
    redactor: &dyn Redactor,
    id: &str,
    budget: Budget,
) -> io::Result<Option<Call>> {
    let clean = |text: &str| elide_middle(&redactor.stored(text).text, STEP_MAX);
    let mut spans = Spans::new(r, engine, end, budget);
    while let Some(span) = spans.next_span()? {
        let result = span.parts.iter().find_map(|p| match p {
            Part::Result {
                id: of,
                text,
                failed,
            } if of == id => Some((clean(text), *failed)),
            _ => None,
        });
        let call = span.parts.iter().find_map(|p| match p {
            Part::Call {
                id: of,
                tool,
                input,
                ..
            } if of == id => Some((tool.clone(), clean(input))),
            _ => None,
        });
        if let Some((tool, input)) = call {
            return Ok(Some(Call {
                id: id.to_string(),
                tool,
                input,
                failed: result.as_ref().is_some_and(|(_, f)| *f),
                result: result.map(|(t, _)| t),
            }));
        }
    }
    Ok(None)
}

/// A page's cursor: the record it was read from and the offset before it.
pub fn cursor(file: &str, offset: u64) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{file}:{offset}"))
}

/// The record and offset a cursor names, when it is one.
pub fn read_cursor(cursor: &str) -> Option<(String, u64)> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor.trim())
        .ok()?;
    let text = String::from_utf8(raw).ok()?;
    let (file, offset) = text.rsplit_once(':')?;
    Some((file.to_string(), offset.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::record::ELIDED;
    use crate::memory::Redacted;
    use std::borrow::Cow;

    struct Hides;
    impl Redactor for Hides {
        fn stored<'a>(&self, text: &'a str) -> Redacted<'a> {
            match text.contains("SECRET-") {
                true => Redacted {
                    text: Cow::Owned(text.replace("SECRET-abc", "[secret]")),
                    count: 1,
                },
                false => Redacted {
                    text: Cow::Borrowed(text),
                    count: 0,
                },
            }
        }
    }

    fn ts(n: u32) -> String {
        format!("2026-11-21T23:{:02}:00Z", n % 60)
    }

    fn ask(n: u32, text: &str) -> String {
        json!({"type": "user", "uuid": format!("u{n}"), "timestamp": ts(n),
            "message": {"role": "user", "content": text}})
        .to_string()
    }

    fn reply(n: u32, text: &str) -> String {
        json!({"type": "assistant", "timestamp": ts(n),
            "message": {"stop_reason": "end_turn", "content": [{"type": "text", "text": text}]}})
        .to_string()
    }

    fn call(n: u32, id: &str, command: &str) -> String {
        json!({"type": "assistant", "timestamp": ts(n),
            "message": {"stop_reason": "tool_use", "content": [{"type": "tool_use", "id": id,
                "name": "Bash", "input": {"command": command}}]}})
        .to_string()
    }

    fn result(n: u32, id: &str, text: &str) -> String {
        json!({"type": "user", "timestamp": ts(n),
            "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": text}]}})
        .to_string()
    }

    fn record(lines: &[String]) -> Vec<u8> {
        lines
            .iter()
            .flat_map(|l| format!("{l}\n").into_bytes())
            .collect()
    }

    fn exchanges(n: u32) -> Vec<String> {
        (0..n)
            .flat_map(|i| {
                [
                    ask(i, &format!("question {i}")),
                    reply(i, &format!("answer {i}")),
                ]
            })
            .collect()
    }

    fn asks(p: &Page) -> Vec<String> {
        p.exchanges
            .iter()
            .map(|x| x["ask"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_page_holds_the_newest_exchanges_first_and_says_where_the_next_ends() {
        let r = record(&exchanges(3));
        let p = page(r.as_slice(), Engine::Claude, r.len() as u64, &Hides).unwrap();
        assert_eq!(asks(&p), ["question 2", "question 1", "question 0"]);
        assert_eq!(p.before, None, "the whole record fits");
        let big: Vec<String> = (0..40)
            .flat_map(|i| [ask(i, &"q".repeat(1500)), reply(i, &"a".repeat(2500))])
            .collect();
        let r = record(&big);
        let p = page(r.as_slice(), Engine::Claude, r.len() as u64, &Hides).unwrap();
        assert!(p.exchanges.len() > 1 && p.exchanges.len() < 40);
        assert!(p.exchanges.iter().map(chars_of).sum::<usize>() <= PAGE_CHARS);
        let next = page(r.as_slice(), Engine::Claude, p.before.unwrap(), &Hides).unwrap();
        assert_eq!(
            next.exchanges[0]["at"],
            json!(ts(40 - 1 - p.exchanges.len() as u32))
        );
    }

    #[test]
    fn a_page_always_holds_one_exchange_cut_to_fit() {
        let huge = vec![ask(0, &"q".repeat(50_000)), reply(0, &"a".repeat(80_000))];
        let r = record(&huge);
        let p = page(r.as_slice(), Engine::Claude, r.len() as u64, &Hides).unwrap();
        assert_eq!(p.exchanges.len(), 1);
        assert!(chars_of(&p.exchanges[0]) <= PAGE_CHARS);
        assert!(p.exchanges[0]["answer"].as_str().unwrap().contains(ELIDED));
    }

    #[test]
    fn a_line_across_a_chunk_boundary_is_read_whole_and_a_partial_last_line_is_left_out() {
        let filler = "x".repeat(usize::try_from(CHUNK).unwrap());
        let mut r = record(&[ask(0, "first"), reply(0, &filler), ask(1, "second")]);
        r.extend_from_slice(b"{\"type\":\"assistant\",\"timest");
        let p = page(r.as_slice(), Engine::Claude, r.len() as u64, &Hides).unwrap();
        assert_eq!(asks(&p)[0], "second");
        let found = search(
            r.as_slice(),
            Engine::Claude,
            r.len() as u64,
            &Hides,
            "FIRST",
            Budget::PAGE,
        )
        .unwrap();
        assert_eq!(found.hits.len(), 1, "found across the boundary");
        assert_eq!(found.hits[0].place, "ask");
    }

    #[test]
    fn a_search_never_matches_a_secret_and_stops_at_its_budget_with_a_cursor() {
        let r = record(&[
            ask(0, "deploy with SECRET-abc"),
            reply(0, "deployed"),
            ask(1, "and again"),
            reply(1, "deployed again"),
        ]);
        let all = |q: &str| {
            search(
                r.as_slice(),
                Engine::Claude,
                r.len() as u64,
                &Hides,
                q,
                Budget::PAGE,
            )
            .unwrap()
        };
        assert!(all("SECRET-abc").hits.is_empty());
        assert_eq!(all("[secret]").hits.len(), 1);
        assert_eq!(all("deployed").hits.len(), 2);
        let filler: Vec<String> = (0..3)
            .flat_map(|i| {
                [
                    ask(i, "old"),
                    reply(i, &"z".repeat(usize::try_from(CHUNK).unwrap())),
                ]
            })
            .chain([ask(9, "new"), reply(9, "new")])
            .collect();
        let r = record(&filler);
        let budget = Budget {
            bytes: CHUNK,
            until: None,
        };
        let found = search(
            r.as_slice(),
            Engine::Claude,
            r.len() as u64,
            &Hides,
            "old",
            budget,
        )
        .unwrap();
        assert!(found.before.is_some(), "it says where to search on from");
    }

    #[test]
    fn a_step_gives_its_whole_output_head_and_tail() {
        let out = format!("HEAD{}TAIL SECRET-abc", "m".repeat(30_000));
        let r = record(&[
            ask(0, "run"),
            call(0, "toolu_1", "cargo test"),
            result(1, "toolu_1", &out),
            reply(2, "done"),
        ]);
        let found = step(
            r.as_slice(),
            Engine::Claude,
            r.len() as u64,
            &Hides,
            "toolu_1",
            Budget::PAGE,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (found.tool.as_str(), found.input.as_str()),
            ("Bash", "cargo test")
        );
        let text = found.result.unwrap();
        assert!(text.starts_with("HEAD") && text.ends_with("TAIL [secret]"));
        assert!(text.contains(ELIDED));
        assert!(text.chars().count() <= STEP_MAX);
        assert_eq!(
            step(
                r.as_slice(),
                Engine::Claude,
                r.len() as u64,
                &Hides,
                "nope",
                Budget::PAGE
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn a_cursor_round_trips_and_junk_is_none() {
        let c = cursor("16777220-4242", 1234);
        assert_eq!(read_cursor(&c), Some(("16777220-4242".into(), 1234)));
        assert_eq!(read_cursor("not a cursor!"), None);
        assert_eq!(read_cursor(&cursor("x", 0).replace('A', "")), None);
    }

    #[test]
    fn a_codex_rollout_pages_through_its_own_reader() {
        let said = |n: u32, role: &str, kind: &str, text: &str| {
            json!({"timestamp": ts(n), "type": "response_item", "payload": {"type": "message",
                "role": role, "content": [{"type": kind, "text": text}]}})
            .to_string()
        };
        let r = record(&[
            said(0, "user", "input_text", "fix the build"),
            said(1, "assistant", "output_text", "fixed"),
            said(2, "user", "input_text", "now test it"),
        ]);
        let p = page(r.as_slice(), Engine::Codex, r.len() as u64, &Hides).unwrap();
        assert_eq!(asks(&p), ["now test it", "fix the build"]);
    }
}

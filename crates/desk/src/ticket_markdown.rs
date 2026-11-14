//! A synced ticket's description as Desk markdown: a Jira description read
//! as Atlassian Document Format (ADF), and a Linear description's issue
//! links and collapsibles.
//!
//! What can't be said in plain markdown is said with remark-directive
//! syntax, which Desk draws and which reads as text where it isn't drawn:
//!
//! - `:mention[Name]`, `:status[Done]{tone=success}`, `:date[2026-11-30]`;
//! - `:::panel{tone=info}` … `:::` and `:::details[Title]` … `:::`, an outer
//!   one fenced with more colons than any inside it;
//! - `![name](desk-file:ID)`: a picture or file of the ticket's own, by the
//!   id of its file row;
//! - `[KEY](https://…/browse/KEY)`: an issue, which Desk shows as a chip.
//!
//! Someone else's words are escaped, so none of them opens a directive, a
//! fence, a list or a link by accident.

use std::collections::HashMap;

use serde_json::Value;
use uuid::Uuid;

use crate::board_sync::{parse_issue_key, safe_href};

/// A file of the ticket's, which an ADF picture can stand for: found by
/// its name, since an ADF `media` node's id is the Media API's, never the
/// attachment's.
#[derive(Debug, Clone, Copy)]
pub struct AdfFile<'a> {
    /// Its row's id, which the page reads it by.
    pub id: Uuid,
    pub name: &'a str,
}

/// An ADF document (`{"type":"doc",…}`) as Desk markdown, its pictures
/// found among `files`; a link to an issue on the site of `own` (the
/// ticket's own address) a ticket chip.
pub fn adf_markdown(doc: &Value, files: &[AdfFile<'_>], own: &str) -> String {
    let cx = Cx {
        files,
        cell: false,
        site: host_of(own).unwrap_or_default(),
    };
    join_blocks(&blocks(children(doc), &cx)).trim().to_string()
}

#[derive(Clone, Copy)]
struct Cx<'a> {
    files: &'a [AdfFile<'a>],
    /// Inside a table cell: one line, no containers.
    cell: bool,
    /// The ticket's own site's host, lowercase: links to its issues are
    /// chips.
    site: &'a str,
}

impl Cx<'_> {
    /// The issue key a link to `href` makes a chip of: one on the ticket's
    /// own site. Only an `https` site has one, so an `http`-only Jira
    /// Server's links stay links: it fails closed.
    fn chip(&self, href: &str) -> Option<String> {
        browse_key(href).filter(|_| {
            !self.site.is_empty()
                && host_of(href).is_some_and(|h| h.eq_ignore_ascii_case(self.site))
        })
    }

    /// `href` as a link writes it here: in a cell, a `|` would end the
    /// cell, so it is percent-encoded.
    fn href(&self, href: &str) -> String {
        match self.cell {
            true => href.replace('|', "%7C"),
            false => href.to_string(),
        }
    }
}

/// An `https` address's host.
fn host_of(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then_some(host)
}

fn children(v: &Value) -> &[Value] {
    v["content"].as_array().map_or(&[], Vec::as_slice)
}

fn kind(v: &Value) -> &str {
    v["type"].as_str().unwrap_or_default()
}

fn attr<'v>(v: &'v Value, name: &str) -> &'v Value {
    &v["attrs"][name]
}

fn attr_str<'v>(v: &'v Value, name: &str) -> &'v str {
    attr(v, name).as_str().unwrap_or_default()
}

// ── Escaping ─────────────────────────────────────────────────────────────

/// `text` as markdown that reads as just that text: nothing in it opens
/// emphasis, code, a link, a directive or (at a line's start, when
/// `line_start`) a heading, quote, list or fence. In a table cell its `|`s
/// are escaped too.
fn escape(text: &str, line_start: bool, cell: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    let mut at_start = line_start;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if at_start {
            // Spaces in front would make it code: they go.
            if c == ' ' || c == '\t' {
                i += 1;
                continue;
            }
            at_start = false;
            if matches!(c, '#' | '>' | '+' | '-' | '=') {
                out.push('\\');
                out.push(c);
                i += 1;
                continue;
            }
            if c.is_ascii_digit() {
                let mut j = i;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                if j < chars.len() && matches!(chars[j], '.' | ')') {
                    out.extend(&chars[i..j]);
                    out.push('\\');
                    out.push(chars[j]);
                    i = j + 1;
                    continue;
                }
            }
        }
        let prev = i.checked_sub(1).map(|p| chars[p]);
        let next = chars.get(i + 1).copied();
        let escaped = match c {
            // `{` after a directive would be read as its attributes.
            '\\' | '`' | '*' | '[' | ']' | '<' | '~' | '{' => true,
            '|' => cell,
            // Inside a word it is only a word's; at its edge, emphasis.
            '_' => {
                !(prev.is_some_and(char::is_alphanumeric)
                    && next.is_some_and(char::is_alphanumeric))
            }
            // `:name` is a directive, `::` a container's start.
            ':' => next.is_some_and(|n| n.is_ascii_alphabetic() || n == ':'),
            // What could read as an entity.
            '&' => next.is_some_and(|n| n.is_ascii_alphanumeric() || n == '#'),
            _ => false,
        };
        if escaped {
            out.push('\\');
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `text` as a directive's or link's label: on one line, escaped.
fn label(text: &str, cell: bool) -> String {
    escape(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        false,
        cell,
    )
}

/// A run of `mark` longer than any run of it in `text`, and at least `min`.
fn fence_for(text: &str, mark: char, min: usize) -> String {
    let longest = text.split(|c| c != mark).map(str::len).max().unwrap_or(0);
    mark.to_string().repeat(min.max(longest + 1))
}

/// `href` when a link may keep it: `https`, and nothing in it that would
/// end the link.
fn https(href: &str) -> Option<&str> {
    let href = href.trim();
    safe_href(href, true).then_some(href)
}

/// The issue key an `https` address to `…/browse/KEY` names.
pub fn browse_key(href: &str) -> Option<String> {
    let rest = href.strip_prefix("https://")?;
    let (_, after) = rest.split_once("/browse/")?;
    let seg = after.split(['/', '?', '#']).next()?;
    let key = parse_issue_key(seg)?;
    (key == seg).then_some(key)
}

// ── Inline content ───────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Mark {
    Strong,
    Em,
    Strike,
}

impl Mark {
    fn delim(self) -> &'static str {
        match self {
            Mark::Strong => "**",
            Mark::Em => "*",
            Mark::Strike => "~~",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Piece {
    /// Someone's words, escaped when written.
    Text(String),
    /// Code, written as a span.
    Code(String),
    /// Markdown already made (a mention, a status, a picture).
    Made(String),
    Break,
}

#[derive(Clone, Debug)]
struct Item {
    piece: Piece,
    marks: Vec<Mark>,
    link: Option<String>,
}

/// An inline node's marks: the ones markdown says, in one order, and its
/// link's address when it is `https`; whether it is code.
fn marks_of(v: &Value) -> (Vec<Mark>, Option<String>, bool) {
    let mut marks = vec![];
    let mut link = None;
    let mut code = false;
    for m in v["marks"].as_array().into_iter().flatten() {
        match kind(m) {
            "strong" => marks.push(Mark::Strong),
            "em" => marks.push(Mark::Em),
            "strike" => marks.push(Mark::Strike),
            "code" => code = true,
            "link" => link = https(attr_str(m, "href")).map(str::to_string),
            _ => {}
        }
    }
    marks.sort();
    marks.dedup();
    (marks, link, code)
}

/// The text a node holds, all of it, as plain words.
fn text_of(v: &Value) -> String {
    match kind(v) {
        "text" => v["text"].as_str().unwrap_or_default().to_string(),
        "hardBreak" => "\n".into(),
        "mention" => attr_str(v, "text").to_string(),
        "emoji" => emoji_text(v),
        "status" => attr_str(v, "text").to_string(),
        "date" => date_text(v).unwrap_or_default(),
        "inlineCard" | "blockCard" | "embedCard" => attr_str(v, "url").to_string(),
        "media" | "mediaInline" => attr_str(v, "alt").to_string(),
        _ => match v["content"].as_array() {
            Some(c) => c
                .iter()
                .map(text_of)
                .collect::<Vec<_>>()
                .join(match is_block_list(c) {
                    true => "\n",
                    false => "",
                }),
            None => attr_str(v, "text").to_string(),
        },
    }
}

fn is_block_list(c: &[Value]) -> bool {
    c.iter().any(|n| !is_inline(n))
}

fn is_inline(v: &Value) -> bool {
    matches!(
        kind(v),
        "text"
            | "hardBreak"
            | "mention"
            | "emoji"
            | "status"
            | "date"
            | "inlineCard"
            | "mediaInline"
            | "placeholder"
            | "inlineExtension"
    )
}

fn emoji_text(v: &Value) -> String {
    match attr_str(v, "text") {
        "" => attr_str(v, "shortName").to_string(),
        t => t.to_string(),
    }
}

fn date_text(v: &Value) -> Option<String> {
    let ms = match attr(v, "timestamp") {
        Value::String(s) => s.trim().parse::<i64>().ok()?,
        Value::Number(n) => n.as_i64()?,
        _ => return None,
    };
    let at = chrono::DateTime::from_timestamp_millis(ms)?;
    Some(at.date_naive().format("%Y-%m-%d").to_string())
}

/// What a Jira status lozenge's colour says, as a Desk tone.
fn tone_of_color(color: &str) -> &'static str {
    match color {
        "blue" => "info",
        "green" => "success",
        "yellow" => "warning",
        "red" => "danger",
        "purple" => "purple",
        _ => "neutral",
    }
}

/// What a panel's type says, as a Desk tone.
fn tone_of_panel(panel: &str) -> &'static str {
    match panel {
        "info" => "info",
        "note" => "purple",
        "success" | "tip" => "success",
        "warning" => "warning",
        "error" => "danger",
        _ => "neutral",
    }
}

/// A picture or file of the ticket's: by its row's id when it is one of
/// `files` (by its name), else its name.
fn media(v: &Value, cx: &Cx<'_>) -> String {
    let alt = attr_str(v, "alt").trim();
    if attr_str(v, "type") == "external" {
        return match https(attr_str(v, "url")) {
            Some(url) => format!(
                "[{}]({})",
                label(if alt.is_empty() { url } else { alt }, cx.cell),
                cx.href(url)
            ),
            None => escape(alt, false, cx.cell),
        };
    }
    // Its `id` is the Media API's, never the attachment's: it is found by
    // its name.
    let found = cx.files.iter().find(|f| !alt.is_empty() && f.name == alt);
    match found {
        Some(f) => {
            let name = if alt.is_empty() { f.name } else { alt };
            format!("![{}](desk-file:{})", label(name, cx.cell), f.id)
        }
        None if alt.is_empty() => "attachment".into(),
        None => escape(alt, false, cx.cell),
    }
}

/// `nodes` (inline ones) as a flat run of pieces, each with its marks.
fn flatten(nodes: &[Value], cx: &Cx<'_>, out: &mut Vec<Item>) {
    for v in nodes {
        let (marks, link, code) = marks_of(v);
        let piece = match kind(v) {
            "text" => {
                let t = v["text"].as_str().unwrap_or_default();
                if t.is_empty() {
                    continue;
                }
                // A newline in a text node is a line break.
                let mut first = true;
                for part in t.split('\n') {
                    if !first {
                        out.push(Item {
                            piece: Piece::Break,
                            marks: vec![],
                            link: None,
                        });
                    }
                    first = false;
                    if !part.is_empty() {
                        out.push(Item {
                            piece: match code {
                                true => Piece::Code(part.into()),
                                false => Piece::Text(part.into()),
                            },
                            marks: marks.clone(),
                            link: link.clone(),
                        });
                    }
                }
                continue;
            }
            "hardBreak" => Piece::Break,
            "emoji" => Piece::Text(emoji_text(v)),
            "mention" => {
                let name = attr_str(v, "text").trim().trim_start_matches('@').trim();
                let name = if name.is_empty() { "someone" } else { name };
                Piece::Made(format!(":mention[{}]", label(name, cx.cell)))
            }
            "status" => {
                let t = attr_str(v, "text").trim();
                if t.is_empty() {
                    continue;
                }
                Piece::Made(format!(
                    ":status[{}]{{tone={}}}",
                    label(t, cx.cell),
                    tone_of_color(attr_str(v, "color"))
                ))
            }
            "date" => match date_text(v) {
                Some(d) => Piece::Made(format!(":date[{d}]")),
                None => continue,
            },
            "inlineCard" => {
                let url = attr_str(v, "url");
                match (https(url), cx.chip(url)) {
                    (Some(url), Some(key)) => Piece::Made(format!("[{key}]({})", cx.href(url))),
                    (Some(url), None) => Piece::Made(format!("<{}>", cx.href(url))),
                    _ => {
                        let t = v["attrs"]["data"]["name"]
                            .as_str()
                            .or_else(|| v["attrs"]["data"]["url"].as_str())
                            .unwrap_or(url);
                        if t.is_empty() {
                            continue;
                        }
                        Piece::Text(t.into())
                    }
                }
            }
            "mediaInline" => Piece::Made(media(v, cx)),
            // An empty editor's hint, never words of the issue's.
            "placeholder" => continue,
            // Anything else keeps its words.
            _ => match v["content"].as_array() {
                Some(c) => {
                    flatten(c, cx, out);
                    continue;
                }
                None => {
                    let t = text_of(v);
                    if t.is_empty() {
                        continue;
                    }
                    Piece::Text(t)
                }
            },
        };
        out.push(Item { piece, marks, link });
    }
}

/// `nodes` (inline ones) as one paragraph's markdown. A line break is a
/// markdown one, or (in a cell or heading, `one_line`) a space.
fn inlines(nodes: &[Value], cx: &Cx<'_>, one_line: bool) -> String {
    let mut items = vec![];
    flatten(nodes, cx, &mut items);
    // Next to each other with the same marks: one run.
    let mut merged: Vec<Item> = vec![];
    for it in items {
        if let (Some(last), Piece::Text(t)) = (merged.last_mut(), &it.piece) {
            if let Piece::Text(lt) = &mut last.piece {
                if last.marks == it.marks && last.link == it.link {
                    lt.push_str(t);
                    continue;
                }
            }
        }
        merged.push(it);
    }
    let mut out = String::new();
    let mut i = 0;
    while i < merged.len() {
        match merged[i].link.clone() {
            Some(href) => {
                let mut j = i;
                while j < merged.len()
                    && merged[j].link.as_deref() == Some(href.as_str())
                    && merged[j].piece != Piece::Break
                {
                    j += 1;
                }
                let group = &merged[i..j];
                let words: String = group
                    .iter()
                    .map(|it| match &it.piece {
                        Piece::Text(t) | Piece::Code(t) => t.as_str(),
                        _ => "",
                    })
                    .collect();
                let words = words.trim();
                let dest = cx.href(&href);
                match cx.chip(&href) {
                    // A link to an issue, its words its address or key: the key.
                    Some(key) if words == href || words.eq_ignore_ascii_case(&key) => {
                        out.push_str(&format!("[{key}]({dest})"))
                    }
                    _ => {
                        let mut inner = String::new();
                        marked(group, &mut inner, false, cx.cell, one_line);
                        match inner.trim().is_empty() {
                            true => {}
                            false => out.push_str(&format!("[{}]({dest})", inner.trim())),
                        }
                    }
                }
                i = j;
            }
            None => {
                let mut j = i;
                while j < merged.len() && merged[j].link.is_none() {
                    j += 1;
                }
                let start = out.is_empty();
                let mut part = String::new();
                marked(&merged[i..j], &mut part, start, cx.cell, one_line);
                out.push_str(&part);
                i = j;
            }
        }
    }
    let out = match one_line {
        true => out.split_whitespace().collect::<Vec<_>>().join(" "),
        false => out
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("  \n"),
    };
    out.trim().to_string()
}

/// Writes `items` onto `out`, their marks opened and closed around them:
/// spaces at a run's edge go outside its marks, and a mark around nothing
/// is never written.
fn marked(items: &[Item], out: &mut String, start: bool, cell: bool, one_line: bool) {
    let mut open: Vec<(Mark, usize)> = vec![];
    let close_to = |out: &mut String, open: &mut Vec<(Mark, usize)>, keep: usize| {
        while open.len() > keep {
            let (m, at) = open.pop().expect("an open mark");
            let d = m.delim();
            if out[at + d.len()..].trim().is_empty() {
                out.replace_range(at..at + d.len(), "");
                continue;
            }
            let trimmed = out.trim_end_matches([' ', '\t']).len();
            let tail = out.split_off(trimmed);
            out.push_str(d);
            out.push_str(&tail);
        }
    };
    for it in items {
        if it.piece == Piece::Break {
            close_to(out, &mut open, 0);
            out.push_str(if one_line { " " } else { "\n" });
            continue;
        }
        let keep = open
            .iter()
            .zip(&it.marks)
            .take_while(|((a, _), b)| a == *b)
            .count();
        close_to(out, &mut open, keep);
        let body = match &it.piece {
            Piece::Text(t) => t.as_str(),
            _ => "",
        };
        // Spaces before the words go before the marks that open on them.
        let lead = body.len() - body.trim_start().len();
        if it.marks.len() > open.len() && lead > 0 {
            out.push(' ');
        }
        let body = match it.marks.len() > open.len() {
            true => &body[lead..],
            false => body,
        };
        for m in &it.marks[open.len()..] {
            open.push((*m, out.len()));
            out.push_str(m.delim());
        }
        let line_start = (out.is_empty() && start) || out.ends_with('\n');
        match &it.piece {
            Piece::Text(_) => out.push_str(&escape(body, line_start, cell)),
            Piece::Code(c) => out.push_str(&code_span(c, cell)),
            Piece::Made(m) => {
                // A directive needs a word start: right after a letter or
                // digit Desk reads it as words (`Owner:mention[Ann]`), so a
                // space goes between. Only a directive: a picture's alt can
                // start `:)`.
                let directive = [":mention[", ":status[", ":date["]
                    .iter()
                    .any(|d| m.starts_with(d));
                let after_word = out.chars().next_back().is_some_and(char::is_alphanumeric);
                if directive && after_word {
                    out.push(' ');
                }
                out.push_str(m)
            }
            Piece::Break => {}
        }
    }
    close_to(out, &mut open, 0);
}

/// `code` as a code span, fenced with more backticks than any run in it.
fn code_span(code: &str, cell: bool) -> String {
    let code = match cell {
        true => code.replace('|', "\\|"),
        false => code.to_string(),
    };
    if code.trim().is_empty() {
        return String::new();
    }
    let fence = fence_for(&code, '`', 1);
    let pad = match code.starts_with('`') || code.ends_with('`') {
        true => " ",
        false => "",
    };
    format!("{fence}{pad}{code}{pad}{fence}")
}

// ── Blocks ───────────────────────────────────────────────────────────────

/// A block's markdown, and whether it is a list (which follows a line in a
/// list item without a blank line between).
struct Block {
    text: String,
    list: bool,
}

fn join_blocks(blocks: &[Block]) -> String {
    let mut out = String::new();
    for (n, b) in blocks.iter().enumerate() {
        if n > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&b.text);
    }
    out
}

/// Blocks as one list item's content: a list right under its first line.
fn join_item(blocks: &[Block]) -> String {
    let mut out = String::new();
    for (n, b) in blocks.iter().enumerate() {
        if n > 0 {
            out.push_str(if b.list { "\n" } else { "\n\n" });
        }
        out.push_str(&b.text);
    }
    out
}

/// `text` as a list item behind `marker`, its other lines under its first.
fn item(marker: &str, text: &str) -> String {
    let pad = " ".repeat(marker.chars().count() + 1);
    let mut out = String::new();
    for (n, l) in text.lines().enumerate() {
        match n {
            0 => {
                out.push_str(marker);
                if !l.is_empty() {
                    out.push(' ');
                    out.push_str(l);
                }
            }
            _ => {
                out.push('\n');
                if !l.is_empty() {
                    out.push_str(&pad);
                    out.push_str(l);
                }
            }
        }
    }
    if out.is_empty() {
        out.push_str(marker);
    }
    out
}

/// `inner` in a container directive `open` (`panel{tone=info}`), fenced
/// with more colons than any fence inside it.
fn container(open: &str, inner: &str) -> String {
    let longest = inner
        .lines()
        .map(|l| l.trim_start().chars().take_while(|c| *c == ':').count())
        .max()
        .unwrap_or(0);
    let fence = ":".repeat(3.max(longest + 1));
    format!("{fence}{open}\n{inner}\n{fence}")
}

fn blocks(nodes: &[Value], cx: &Cx<'_>) -> Vec<Block> {
    let mut out = vec![];
    let mut i = 0;
    while i < nodes.len() {
        // Inline nodes where a block belongs: one paragraph of them.
        if is_inline(&nodes[i]) {
            let mut j = i;
            while j < nodes.len() && is_inline(&nodes[j]) {
                j += 1;
            }
            let text = inlines(&nodes[i..j], cx, cx.cell);
            if !text.is_empty() {
                out.push(Block { text, list: false });
            }
            i = j;
            continue;
        }
        if let Some(b) = block(&nodes[i], cx) {
            if !b.text.trim().is_empty() {
                out.push(b);
            }
        }
        i += 1;
    }
    out
}

fn block(v: &Value, cx: &Cx<'_>) -> Option<Block> {
    let plain = |text: String| Some(Block { text, list: false });
    match kind(v) {
        "paragraph" => plain(inlines(children(v), cx, cx.cell)),
        "heading" => {
            let text = inlines(children(v), cx, true);
            if text.is_empty() {
                return None;
            }
            match cx.cell {
                true => plain(text),
                false => {
                    let level = attr(v, "level").as_u64().unwrap_or(1).clamp(1, 6) as usize;
                    plain(format!("{} {text}", "#".repeat(level)))
                }
            }
        }
        "bulletList" => list(v, cx, |_| "*".into()),
        "orderedList" => {
            let start = attr(v, "order").as_u64().unwrap_or(1).min(999_999_999);
            list(v, cx, |n| format!("{}.", start + n as u64))
        }
        "taskList" | "decisionList" => {
            let mut items: Vec<String> = vec![];
            for c in children(v) {
                match kind(c) {
                    "taskItem" | "decisionItem" => {
                        let done = attr_str(c, "state") == "DONE";
                        let marker = match (kind(c), done) {
                            ("decisionItem", _) => "-",
                            (_, true) => "- [x]",
                            (_, false) => "- [ ]",
                        };
                        let text = inlines(children(c), cx, cx.cell);
                        items.push(item(marker, &text));
                    }
                    // A list inside: under the item before it.
                    _ => {
                        let Some(inner) = block(c, cx) else { continue };
                        let nested = inner
                            .text
                            .lines()
                            .map(|l| match l.is_empty() {
                                true => String::new(),
                                false => format!("  {l}"),
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        match items.last_mut() {
                            Some(last) => {
                                last.push('\n');
                                last.push_str(&nested);
                            }
                            None => items.push(inner.text),
                        }
                    }
                }
            }
            Some(Block {
                text: one_or_lines(&items, cx),
                list: !cx.cell,
            })
        }
        "blockquote" => {
            let inner = join_blocks(&blocks(children(v), cx));
            match cx.cell {
                true => plain(inner),
                false => plain(
                    inner
                        .lines()
                        .map(|l| match l.is_empty() {
                            true => ">".to_string(),
                            false => format!("> {l}"),
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            }
        }
        "codeBlock" => {
            let code: String = children(v).iter().map(text_of).collect();
            if cx.cell {
                return plain(code_span(
                    &code.split_whitespace().collect::<Vec<_>>().join(" "),
                    true,
                ));
            }
            let lang: String = attr_str(v, "language")
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '#' | '.' | '-'))
                .take(30)
                .collect();
            let fence = fence_for(&code, '`', 3);
            let code = code.trim_end_matches('\n');
            plain(format!("{fence}{lang}\n{code}\n{fence}"))
        }
        "panel" => {
            let inner = join_blocks(&blocks(children(v), cx));
            if inner.trim().is_empty() || cx.cell {
                return plain(inner);
            }
            let tone = tone_of_panel(attr_str(v, "panelType"));
            plain(container(&format!("panel{{tone={tone}}}"), &inner))
        }
        "expand" | "nestedExpand" => {
            let inner = join_blocks(&blocks(children(v), cx));
            let title = attr_str(v, "title").trim();
            if cx.cell {
                return plain(format!("{} {inner}", label(title, true)));
            }
            let open = match title.is_empty() {
                true => "details".to_string(),
                false => format!("details[{}]", label(title, false)),
            };
            plain(container(&open, &inner))
        }
        "rule" if !cx.cell => plain("---".into()),
        "rule" => None,
        "mediaSingle" | "mediaGroup" => {
            // Each picture, then its caption's words.
            let parts: Vec<String> = children(v)
                .iter()
                .filter_map(|m| match kind(m) {
                    "media" => Some(media(m, cx)),
                    "caption" => Some(inlines(children(m), cx, true)),
                    _ => None,
                })
                .filter(|p| !p.is_empty())
                .collect();
            plain(parts.join(if cx.cell { " " } else { "\n\n" }))
        }
        "media" => plain(media(v, cx)),
        "table" => table(v, cx),
        "blockCard" | "embedCard" => {
            let url = attr_str(v, "url");
            match (https(url), cx.chip(url)) {
                (Some(url), Some(key)) => plain(format!("[{key}]({})", cx.href(url))),
                (Some(url), None) => plain(format!("<{}>", cx.href(url))),
                _ => plain(escape(url, !cx.cell, cx.cell)),
            }
        }
        // Anything else keeps its words: its blocks, or its text.
        _ => match v["content"].as_array() {
            Some(c) => {
                let inner = blocks(c, cx);
                plain(join_blocks(&inner))
            }
            None => {
                let t = text_of(v);
                plain(escape(t.trim(), !cx.cell, cx.cell))
            }
        },
    }
}

/// Items as a list's lines; in a cell, on one line.
fn one_or_lines(items: &[String], cx: &Cx<'_>) -> String {
    match cx.cell {
        true => items.join(" "),
        false => items.join("\n"),
    }
}

/// A bullet or numbered list, each item behind `marker(n)` and its other
/// lines under its words: two spaces under `*`, the marker's width under
/// `1.`, which is what nests a list under it.
fn list(v: &Value, cx: &Cx<'_>, marker: impl Fn(usize) -> String) -> Option<Block> {
    let items: Vec<String> = children(v)
        .iter()
        .enumerate()
        .map(|(n, li)| {
            let content = match kind(li) {
                "listItem" => join_item(&blocks(children(li), cx)),
                _ => join_item(&blocks(std::slice::from_ref(li), cx)),
            };
            item(&marker(n), &content)
        })
        .collect();
    Some(Block {
        text: one_or_lines(&items, cx),
        list: !cx.cell,
    })
}

/// A table as a GFM table: its first row the header, each cell's content
/// on one line, short rows filled out.
fn table(v: &Value, cx: &Cx<'_>) -> Option<Block> {
    let cell_cx = Cx { cell: true, ..*cx };
    let rows: Vec<Vec<String>> = children(v)
        .iter()
        .filter(|r| kind(r) == "tableRow")
        .map(|r| {
            children(r)
                .iter()
                .map(|c| {
                    let text = join_blocks(&blocks(children(c), &cell_cx));
                    text.split_whitespace().collect::<Vec<_>>().join(" ")
                })
                .collect()
        })
        .collect();
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return None;
    }
    if cx.cell {
        let words: Vec<String> = rows
            .into_iter()
            .flatten()
            .filter(|c| !c.is_empty())
            .collect();
        return Some(Block {
            text: words.join(" "),
            list: false,
        });
    }
    let line = |cells: &[String]| {
        let mut l = String::from("|");
        for n in 0..width {
            let c = cells.get(n).map(String::as_str).unwrap_or("");
            l.push(' ');
            l.push_str(c);
            l.push_str(if c.is_empty() { "|" } else { " |" });
        }
        l
    };
    let mut out = vec![line(&rows[0])];
    out.push(format!("|{}", " --- |".repeat(width)));
    out.extend(rows[1..].iter().map(|r| line(r)));
    Some(Block {
        text: out.join("\n"),
        list: false,
    })
}

// ── Linear ───────────────────────────────────────────────────────────────

/// Whether `line` opens or closes a code fence; the fence when it does.
fn fence_mark(line: &str) -> Option<String> {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return None;
    }
    let c = t.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let run: String = t.chars().take_while(|x| *x == c).collect();
    (run.len() >= 3).then_some(run)
}

/// The lines of `md`, each with whether it is inside a code fence (its
/// fence lines count as inside).
fn fenced_lines(md: &str) -> Vec<(&str, bool)> {
    let mut fence: Option<String> = None;
    md.split('\n')
        .map(|l| match (&fence, fence_mark(l)) {
            (None, Some(f)) => {
                fence = Some(f);
                (l, true)
            }
            (Some(f), Some(g)) if g.starts_with(f.as_str()) && l.trim() == g => {
                fence = None;
                (l, true)
            }
            (Some(_), _) => (l, true),
            (None, None) => (l, false),
        })
        .collect()
}

/// A Linear issue's address's key, when `url` is one
/// (`https://linear.app/<workspace>/issue/KEY[/slug]`).
fn linear_issue_key(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://linear.app/")?;
    let (ws, rest) = rest.split_once('/')?;
    if ws.is_empty()
        || !ws
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return None;
    }
    let rest = rest.strip_prefix("issue/")?;
    let seg = rest.split(['/', '?', '#']).next()?;
    let key = parse_issue_key(seg)?;
    (key == seg && safe_href(url, true)).then_some(key)
}

/// A Linear description as Desk markdown: `+++ Title` … `+++` collapsibles
/// become `:::details[Title]` … `:::`, and Linear issue addresses and bare
/// keys of `key`'s own team become `[KEY](address)` links, `url` (the
/// issue's own address) giving the workspace's. Code is never touched, and
/// reading it again changes nothing.
pub fn linear_markdown(md: &str, key: &str, url: &str) -> String {
    let base = linear_issue_key(url).and_then(|_| {
        let at = url.find("/issue/")?;
        Some(url[..at + "/issue/".len()].to_string())
    });
    let team = key.rsplit_once('-').map(|(t, _)| t.to_string());
    let lines = fenced_lines(md);
    // The collapsibles that close, by their lines; how many colons each takes.
    let mut opens: Vec<(usize, usize)> = vec![];
    let mut pairs: Vec<(usize, usize, usize)> = vec![];
    for (n, (l, code)) in lines.iter().enumerate() {
        if *code {
            continue;
        }
        let t = l.trim();
        if t == "+++" {
            if let Some((open, inner)) = opens.pop() {
                let colons = 3.max(inner + 1);
                pairs.push((open, n, colons));
                if let Some(parent) = opens.last_mut() {
                    parent.1 = parent.1.max(colons);
                }
            }
        } else if let Some(title) = t.strip_prefix("+++ ") {
            if !title.trim().is_empty() {
                opens.push((n, 0));
            }
        }
    }
    let opening: HashMap<usize, usize> = pairs.iter().map(|p| (p.0, p.2)).collect();
    let closing: HashMap<usize, usize> = pairs.iter().map(|p| (p.1, p.2)).collect();
    let mut out = Vec::with_capacity(lines.len());
    for (n, (l, code)) in lines.iter().enumerate() {
        if let Some(colons) = opening.get(&n) {
            let title = l.trim().trim_start_matches("+++").trim();
            let title = title
                .replace('\\', "\\\\")
                .replace('[', "\\[")
                .replace(']', "\\]");
            out.push(format!("{}details[{title}]", ":".repeat(*colons)));
        } else if let Some(colons) = closing.get(&n) {
            out.push(":".repeat(*colons));
        } else if *code {
            out.push(l.to_string());
        } else {
            out.push(linear_links(l, team.as_deref(), base.as_deref()));
        }
    }
    out.join("\n")
}

/// One line's Linear issue addresses and bare keys of `team` as links;
/// code spans, links and other addresses as they are.
fn linear_links(line: &str, team: Option<&str>, base: Option<&str>) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let word = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'/' | b'.' | b'@');
    let mut spans = CodeSpans::of(line);
    // Where the last run read as a scheme, and not one, ends: a word inside
    // it (`a+a+a`) is never read as a scheme again.
    let mut no_scheme_until = 0;
    while i < b.len() {
        let c = b[i];
        // An escaped character stays escaped.
        if c == b'\\' && i + 1 < b.len() {
            let n = line[i + 1..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&line[i..i + 1 + n]);
            i += 1 + n;
            continue;
        }
        if c == b'`' {
            let to = spans.skip(line, i);
            out.push_str(&line[i..to]);
            i = to;
            continue;
        }
        if c == b'[' {
            if let Some((text, dest, to)) = md_link(line, i) {
                match linear_issue_key(dest) {
                    Some(key) if text == dest => out.push_str(&format!("[{key}]({dest})")),
                    _ => out.push_str(&line[i..to]),
                }
                i = to;
                continue;
            }
        }
        if c == b'<' {
            // An autolink holds no space and no `<`: the look stops there.
            let end = line[i + 1..]
                .bytes()
                .take(LOOK_MAX)
                .position(|x| x == b'>' || x == b'<' || x.is_ascii_whitespace())
                .map(|p| p + 1)
                .filter(|p| b[i + p] == b'>');
            if let Some(end) = end {
                let inner = &line[i + 1..i + end];
                if inner.contains("://") {
                    match linear_issue_key(inner) {
                        Some(key) => out.push_str(&format!("[{key}]({inner})")),
                        None => out.push_str(&line[i..=i + end]),
                    }
                    i += end + 1;
                    continue;
                }
            }
        }
        let at_word = i == 0 || !word(b[i - 1]);
        if at_word && c.is_ascii_alphabetic() {
            // An address: a link when it is a Linear issue's, else as it is.
            let scheme = match i < no_scheme_until {
                true => 0,
                false => line[i..]
                    .bytes()
                    .take_while(|x| x.is_ascii_alphanumeric() || matches!(x, b'+' | b'.' | b'-'))
                    .count(),
            };
            if scheme > 0 && !line[i + scheme..].starts_with("://") {
                no_scheme_until = i + scheme;
            }
            if scheme > 0 && line[i + scheme..].starts_with("://") {
                let len = line[i..]
                    .find(|ch: char| ch.is_whitespace() || ch == '<')
                    .unwrap_or(line.len() - i);
                let mut url = &line[i..i + len];
                url = url.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '\'', '"']);
                match linear_issue_key(url) {
                    Some(key) => out.push_str(&format!("[{key}]({url})")),
                    None => out.push_str(url),
                }
                i += url.len();
                continue;
            }
            // A bare key of its own team.
            if let (Some(team), Some(base)) = (team, base) {
                if line[i..].starts_with(team) && line[i + team.len()..].starts_with('-') {
                    let digits = line[i + team.len() + 1..]
                        .bytes()
                        .take_while(u8::is_ascii_digit)
                        .count();
                    let end = i + team.len() + 1 + digits;
                    let after_ok = b
                        .get(end)
                        .is_none_or(|x| !(x.is_ascii_alphanumeric() || matches!(x, b'-' | b'_')));
                    if digits > 0 && after_ok {
                        if let Some(key) =
                            parse_issue_key(&line[i..end]).filter(|k| k == &line[i..end])
                        {
                            out.push_str(&format!("[{key}]({base}{key})"));
                            i = end;
                            continue;
                        }
                    }
                }
            }
            // The rest of the word as it is, so no key is found inside it.
            let len = line[i..]
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                .unwrap_or(line.len() - i);
            out.push_str(&line[i..i + len]);
            i += len;
            continue;
        }
        let n = line[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&line[i..i + n]);
        i += n;
    }
    out
}

/// How far a link's words, its address or an autolink are looked for:
/// past it, the `[` or `<` is only a character, so a line is read once.
const LOOK_MAX: usize = 2048;

/// How deep brackets nest in a link's words or address before it is taken
/// as no link.
const NEST_MAX: usize = 32;

/// The markdown link (or picture's) at `at` (its `[`): its words, its
/// address, and where it ends. Looked for only so far ([`LOOK_MAX`]) and so
/// deep ([`NEST_MAX`]), and an address never holds a `[`.
fn md_link(line: &str, at: usize) -> Option<(&str, &str, usize)> {
    let b = line.as_bytes();
    let limit = b.len().min(at + LOOK_MAX);
    let mut depth = 0;
    let mut i = at;
    let close = loop {
        if i >= limit {
            return None;
        }
        match b[i] {
            b'\\' => i += 1,
            b'[' => {
                depth += 1;
                if depth > NEST_MAX {
                    return None;
                }
            }
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break i;
                }
            }
            _ => {}
        }
        i += 1;
    };
    if b.get(close + 1) != Some(&b'(') {
        return None;
    }
    let limit = b.len().min(close + 1 + LOOK_MAX);
    let mut depth = 0;
    let mut j = close + 1;
    let end = loop {
        if j >= limit {
            return None;
        }
        match b[j] {
            b'\\' => j += 1,
            b'[' => return None,
            b'(' => {
                depth += 1;
                if depth > NEST_MAX {
                    return None;
                }
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    break j;
                }
            }
            _ => {}
        }
        j += 1;
    };
    Some((&line[at + 1..close], &line[close + 2..end], end + 1))
}

/// A line's code spans: each run of backticks closed by the next run as
/// long, found in one read of the line.
struct CodeSpans {
    spans: Vec<(usize, usize)>,
    next: usize,
}

impl CodeSpans {
    fn of(line: &str) -> Self {
        let b = line.as_bytes();
        let mut runs: Vec<(usize, usize)> = vec![];
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'`' {
                let start = i;
                while i < b.len() && b[i] == b'`' {
                    i += 1;
                }
                runs.push((start, i - start));
            } else {
                i += 1;
            }
        }
        // The next run as long as each, from the end.
        let mut later: HashMap<usize, usize> = HashMap::new();
        let mut closer = vec![None; runs.len()];
        for k in (0..runs.len()).rev() {
            closer[k] = later.get(&runs[k].1).copied();
            later.insert(runs[k].1, k);
        }
        let mut spans = vec![];
        let mut k = 0;
        while k < runs.len() {
            match closer[k] {
                Some(m) => {
                    spans.push((runs[k].0, runs[m].0 + runs[m].1));
                    k = m + 1;
                }
                None => k += 1,
            }
        }
        Self { spans, next: 0 }
    }

    /// Where what starts with the backtick at `at` ends: past its code
    /// span, or past its run when it opens none.
    fn skip(&mut self, line: &str, at: usize) -> usize {
        while self.spans.get(self.next).is_some_and(|s| s.0 < at) {
            self.next += 1;
        }
        match self.spans.get(self.next) {
            Some(&(start, end)) if start == at => {
                self.next += 1;
                end
            }
            _ => at + line[at..].bytes().take_while(|x| *x == b'`').count(),
        }
    }
}

/// A description written here as Linear reads it: `:::details[Title]` …
/// `:::` back to `+++ Title` … `+++`. Every other line as it is.
pub fn linear_out(md: &str) -> String {
    let lines = fenced_lines(md);
    // The containers open: each one's colons, and whether it is a details.
    let mut open: Vec<(usize, bool)> = vec![];
    let mut out = Vec::with_capacity(lines.len());
    for (l, code) in lines {
        if code {
            out.push(l.to_string());
            continue;
        }
        let t = l.trim();
        let colons = t.chars().take_while(|c| *c == ':').count();
        if colons >= 3 {
            let rest = &t[colons..];
            if rest.trim().is_empty() {
                if let Some((n, details)) = open.last().copied() {
                    if colons >= n {
                        open.pop();
                        out.push(match details {
                            true => "+++".to_string(),
                            false => l.to_string(),
                        });
                        continue;
                    }
                }
            } else if let Some(title) = rest
                .strip_prefix("details")
                .filter(|t| t.is_empty() || t.starts_with(['[', '{', ' ']))
            {
                let title = title.trim();
                let title = title
                    .strip_prefix('[')
                    .and_then(|x| x.strip_suffix(']'))
                    .unwrap_or("");
                let title = unescape_label(title);
                let title = match title.trim().is_empty() {
                    true => "Details".to_string(),
                    false => title.trim().to_string(),
                };
                open.push((colons, true));
                out.push(format!("+++ {title}"));
                continue;
            } else if rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
                open.push((colons, false));
            }
        }
        out.push(l.to_string());
    }
    out.join("\n")
}

fn unescape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            c => out.push(c),
        }
    }
    out
}

// ── Back to Jira ─────────────────────────────────────────────────────────

/// A description read as ADF and edited here, as Jira's markdown reader
/// takes it: `:mention[Name]` → `@Name`, `:status[T]{…}` → `[T]`,
/// `:date[D]` → `D`, a panel → its content quoted, a collapsible → its
/// title in bold, then its content, a picture of the ticket's → its name.
/// Links, code and everything else as they are.
pub fn jira_out(md: &str) -> String {
    #[derive(Clone, Copy, PartialEq)]
    enum Open {
        Panel,
        Details,
        Other,
    }
    let lines = fenced_lines(md);
    let mut open: Vec<(usize, Open)> = vec![];
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    // A `> ` for each panel open.
    let mut quote = String::new();
    for (l, code) in lines {
        let say = |out: &mut Vec<String>, quote: &str, text: &str| {
            out.push(match text.is_empty() {
                true => quote.trim_end().to_string(),
                false => format!("{quote}{text}"),
            })
        };
        if code {
            say(&mut out, &quote, l);
            continue;
        }
        let t = l.trim();
        let colons = t.chars().take_while(|c| *c == ':').count();
        if colons >= 3 {
            let rest = &t[colons..];
            if rest.trim().is_empty() {
                if let Some((n, kind)) = open.last().copied() {
                    if colons >= n {
                        open.pop();
                        if kind == Open::Panel {
                            quote.truncate(quote.len() - 2);
                        }
                        if kind == Open::Other {
                            out.push(l.to_string());
                        }
                        continue;
                    }
                }
            } else if rest.starts_with("panel") {
                open.push((colons, Open::Panel));
                quote.push_str("> ");
                continue;
            } else if let Some(title) = rest.strip_prefix("details") {
                let title = title
                    .trim()
                    .strip_prefix('[')
                    .and_then(|x| x.strip_suffix(']'))
                    .unwrap_or("")
                    .trim();
                if !title.is_empty() {
                    say(&mut out, &quote, &format!("**{title}**"));
                }
                open.push((colons, Open::Details));
                continue;
            } else if rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
                open.push((colons, Open::Other));
            }
        }
        say(&mut out, &quote, &plain_directives(l));
    }
    out.join("\n")
}

/// One line's text directives and pictures of the ticket's as plain
/// markdown; code spans and escaped characters as they are.
fn plain_directives(line: &str) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut spans = CodeSpans::of(line);
    // The label from `[` at `at` to its `]`, and where it ends.
    let label_at = |at: usize| -> Option<(&str, usize)> {
        let mut j = at + 1;
        while j < b.len().min(at + LOOK_MAX) {
            match b[j] {
                b'\\' => j += 1,
                b']' => return Some((&line[at + 1..j], j + 1)),
                // A label Desk wrote holds no bare `[`.
                b'[' => return None,
                _ => {}
            }
            j += 1;
        }
        None
    };
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && i + 1 < b.len() {
            let n = line[i + 1..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&line[i..i + 1 + n]);
            i += 1 + n;
            continue;
        }
        if c == b'`' {
            let to = spans.skip(line, i);
            out.push_str(&line[i..to]);
            i = to;
            continue;
        }
        if c == b'!' && b.get(i + 1) == Some(&b'[') {
            if let Some((name, dest, to)) = md_link(line, i + 1) {
                if dest.starts_with("desk-file:") {
                    out.push_str(name);
                    i = to;
                    continue;
                }
            }
        }
        let word_before = i > 0 && b[i - 1].is_ascii_alphanumeric();
        if c == b':' && !word_before {
            let rest = &line[i + 1..];
            let found = ["mention", "status", "date"]
                .into_iter()
                .find(|n| rest.starts_with(&format!("{n}[")));
            if let Some(name) = found {
                let at = i + 1 + name.len();
                if let Some((text, mut end)) = label_at(at) {
                    if b.get(end) == Some(&b'{') {
                        if let Some(close) =
                            line[end..].bytes().take(LOOK_MAX).position(|x| x == b'}')
                        {
                            end += close + 1;
                        }
                    }
                    match name {
                        "mention" => out.push_str(&format!("@{text}")),
                        "status" => out.push_str(&format!("[{text}]")),
                        _ => out.push_str(text),
                    }
                    i = end;
                    continue;
                }
            }
        }
        let n = line[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&line[i..i + n]);
        i += n;
    }
    out
}

#[cfg(test)]
#[path = "ticket_markdown_tests.rs"]
mod tests;

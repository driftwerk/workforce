//! Render agent-authored markdown into Slack Block Kit `rich_text` blocks.
//!
//! Slack's legacy `mrkdwn` (the `section` block text format) cannot express
//! lists at all — a leading `-`/`*` renders as a literal character — and its
//! fenced-code handling is unreliable. `rich_text` is Slack's canonical
//! structured format (the one its own WYSIWYG composer emits) and supports,
//! by construction: bulleted / ordered / nested lists, bold / italic / strike
//! / inline-code spans, structured links, fenced code, and block quotes.
//!
//! We therefore parse markdown into an AST with [`pulldown_cmark`] (the same
//! approach as the Jira ADF renderer in `adapters::jira::markdown`) and emit
//! `rich_text` blocks. This is the single rendering path for all outbound
//! Slack prose.
//!
//! ## Mentions
//!
//! Outbound text reaches this module *after*
//! [`super::surface::SlackSurface::rewrite_outbound_text`] has rewritten
//! human-readable `@name` / `#channel` mentions into Slack wire tokens
//! (`<@U123>`, `<#C123|name>`, `<!here>`). Unlike `section` mrkdwn, a
//! `rich_text` `text` element does **not** parse those tokens — it would show
//! them verbatim. So when emitting a text run we split out any such token and
//! emit the matching structured element (`user`, `channel`, `broadcast`,
//! `usergroup`, or `link`). Markdown links (`[label](url)`) come straight from
//! the parser as `link` elements.
//!
//! ## Emoji and bare URLs
//!
//! A `rich_text` `text` element is equally literal about two things the legacy
//! mrkdwn surface auto-rendered: `:shortcode:` emoji (which stay as the raw
//! `:mag:` text) and bare `https://…` URLs (which stay unclickable plain text).
//! So when scanning a text run we also split out `:emoji:` shortcodes into
//! structured `emoji` elements (Slack resolves the name server-side; unknown
//! names harmlessly fall back to literal text) and bare URLs into `link`
//! elements. The emoji match deliberately requires a leading letter (or the two
//! all-symbol names `+1` / `-1`) so timestamps like `00:32:08` are never
//! mistaken for a `:32:` shortcode.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde_json::{json, Map, Value};

/// Max child elements inside one `rich_text` block (Slack-documented).
pub const MAX_RICH_TEXT_ELEMENTS: usize = 50;
/// Max blocks Slack accepts per message.
const MAX_BLOCKS_PER_MESSAGE: usize = 50;
/// Soft cap on the JSON payload of one `chat.postMessage` request, well under
/// Slack's 40KB hard limit to leave room for envelope fields.
const PAYLOAD_BUDGET_BYTES: usize = 35_000;
/// Worst-case bytes consumed by `channel` / `text` / `thread_ts` keys outside
/// the `blocks` array.
const MSG_OVERHEAD_BYTES: usize = 256;
/// Soft cap on a single text run (chars). Over-long prose/code is split into
/// multiple sibling elements so no single block approaches the payload limit.
const TEXT_RUN_LIMIT: usize = 2_900;
/// Slack stops indenting visually past this depth; clamp to avoid rejects.
const MAX_LIST_INDENT: usize = 8;
/// Length cap on the desktop-notification fallback text.
const NOTIFICATION_FALLBACK_LIMIT: usize = 200;

// ── Public API ──────────────────────────────────────────────────────────────

/// Build Block Kit message payloads for `text`. The outer `Vec` is one entry
/// per `chat.postMessage` request — usually one, more only when element or
/// payload limits force a split. Each inner `Vec` is the `blocks` array for
/// that request.
///
/// With `paragraph_spacing`, a blank-line spacer block is inserted between
/// adjacent top-level blocks so paragraphs and sections read with the vertical
/// gap a human leaves in the composer.
pub fn build_messages(text: &str, paragraph_spacing: bool) -> Vec<Vec<Value>> {
    let elements = render_elements(text);
    if elements.is_empty() {
        return Vec::new();
    }
    let blocks = blockify(elements);
    let blocks = if paragraph_spacing {
        space_blocks(blocks)
    } else {
        blocks
    };
    group_into_messages(blocks, paragraph_spacing)
}

/// Plain-text snippet for the `text` field on a Block Kit message — used by
/// Slack desktop / mobile lockscreen notifications when blocks can't render.
///
/// Derived from the parsed element tree so the preview is clean prose: markdown
/// markup (`**`, `[]()`, …) is gone, code blocks are omitted (so we don't leak
/// SQL or shell snippets), and Slack wire tokens render as their display text.
pub fn notification_fallback(text: &str) -> String {
    let elements = render_elements(text);
    let mut buf = String::new();
    for el in &elements {
        collect_preview_text(el, &mut buf);
    }
    let trimmed = buf.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.chars().count() <= NOTIFICATION_FALLBACK_LIMIT {
        return trimmed;
    }
    let cut: String = trimmed.chars().take(NOTIFICATION_FALLBACK_LIMIT).collect();
    format!("{}…", cut.trim_end())
}

/// Accumulate human-readable text from a rendered `rich_text` element for the
/// notification preview. Preformatted (code) elements are skipped entirely;
/// `text` and `link` leaves contribute their visible text; mention tokens are
/// dropped (their resolved names aren't available here and IDs aren't useful in
/// a preview).
fn collect_preview_text(el: &Value, out: &mut String) {
    match el.get("type").and_then(|t| t.as_str()) {
        Some("rich_text_preformatted") => {}
        Some("text") | Some("link") => {
            if let Some(t) = el.get("text").and_then(|t| t.as_str()) {
                out.push_str(t);
            }
        }
        _ => {
            if let Some(children) = el.get("elements").and_then(|e| e.as_array()) {
                for child in children {
                    collect_preview_text(child, out);
                }
                // Separate block-level runs (sections, list items) with a space
                // so words don't run together in the preview.
                out.push(' ');
            }
        }
    }
}

// ── Intermediate tree ────────────────────────────────────────────────────────
//
// pulldown emits a nested list structure, but Slack represents nesting with a
// flat sequence of `rich_text_list` elements carrying an `indent` level rather
// than embedding sub-lists inside items. We therefore parse into this nested
// `Node` tree first, then flatten it (see `flatten`).

#[derive(Debug)]
enum Node {
    /// A paragraph — a run of inline elements.
    Section(Vec<Value>),
    /// A markdown heading — a run of inline elements plus its level (1–6).
    /// Rendered as its own bold section; top-level headings (`#`/`##`) also get
    /// a leading `divider`, so the message breaks into visually separated
    /// sections instead of blending into the prose. Deeper headings stay
    /// lighter (bold only) to preserve hierarchy and avoid divider clutter.
    Heading(Vec<Value>, u8),
    /// A block quote — a run of inline elements.
    Quote(Vec<Value>),
    /// A fenced or indented code block (raw text, no inline parsing).
    Pre(String),
    /// A markdown thematic break (`---` / `***` / `___`). Becomes a top-level
    /// `divider` block — an explicit, full-width separator the agent can drop
    /// between sections, distinct from the implicit divider a top-level heading
    /// already gets.
    Rule,
    /// A bullet or ordered list; each item is its own list of child nodes.
    List {
        ordered: bool,
        items: Vec<Vec<Node>>,
    },
}

// ── Inline style ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Default)]
struct Style {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
}

impl Style {
    fn to_value(self) -> Option<Value> {
        let mut m = Map::new();
        if self.bold {
            m.insert("bold".into(), Value::Bool(true));
        }
        if self.italic {
            m.insert("italic".into(), Value::Bool(true));
        }
        if self.strike {
            m.insert("strike".into(), Value::Bool(true));
        }
        if self.code {
            m.insert("code".into(), Value::Bool(true));
        }
        (!m.is_empty()).then_some(Value::Object(m))
    }
}

fn with_style(mut node: Value, style: Style) -> Value {
    if let Some(s) = style.to_value() {
        if let Some(obj) = node.as_object_mut() {
            obj.insert("style".into(), s);
        }
    }
    node
}

// ── Loose-bullet & entity normalisation ──────────────────────────────────────

/// Bullet glyphs models emit as literal characters instead of a markdown
/// `-`/`*` marker. pulldown_cmark treats none of these as a list marker, so a
/// `• item` line parses as ordinary prose — and a single newline between such
/// lines is a CommonMark soft break (rendered as a space), so the "bullets"
/// collapse onto one run-on line. We rewrite a leading glyph to `- ` so a real
/// list is built instead.
const LITERAL_BULLET_GLYPHS: &[char] = &['•', '‣', '◦', '▪', '∙', '·', '‧'];

/// If `trimmed` opens or closes a fenced code block (`\u{0060}\u{0060}\u{0060}`
/// or `~~~`, three or more), return the fence char. Used to leave bullet glyphs
/// inside code fences untouched.
fn code_fence_marker(trimmed: &str) -> Option<char> {
    let marker = match trimmed.as_bytes().first()? {
        b'`' => '`',
        b'~' => '~',
        _ => return None,
    };
    (trimmed.chars().take_while(|&c| c == marker).count() >= 3).then_some(marker)
}

/// Rewrite leading literal bullet glyphs (`•`, `‣`, …) to a markdown `- `,
/// preserving indentation so nested bullets keep nesting. Lines inside fenced
/// code blocks are left verbatim. Returns the input untouched when there is
/// nothing to rewrite.
fn normalize_loose_bullets(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains(LITERAL_BULLET_GLYPHS) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<char> = None;
    let mut changed = false;
    for line in text.split_inclusive('\n') {
        let (body, nl) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        let trimmed = body.trim_start();
        if let Some(marker) = code_fence_marker(trimmed) {
            match fence {
                Some(open) if open == marker => fence = None,
                Some(_) => {}
                None => fence = Some(marker),
            }
            out.push_str(line);
            continue;
        }
        if fence.is_none() {
            let mut chars = trimmed.chars();
            if let Some(first) = chars.next() {
                let rest = chars.as_str();
                // Require a following space/tab so a mid-prose middot or a glyph
                // glued to text is never treated as a bullet.
                if LITERAL_BULLET_GLYPHS.contains(&first) && rest.starts_with([' ', '\t']) {
                    let indent = &body[..body.len() - trimmed.len()];
                    out.push_str(indent);
                    out.push('-');
                    out.push_str(rest);
                    out.push_str(nl);
                    changed = true;
                    continue;
                }
            }
        }
        out.push_str(line);
    }
    if changed {
        std::borrow::Cow::Owned(out)
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

// ── Markdown → Node tree (the visitor) ───────────────────────────────────────

fn render_elements(text: &str) -> Vec<Value> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    // Rewrite literal `•`-style bullets into markdown `- ` first, so the parser
    // builds a real list instead of a soft-break-collapsed run-on line.
    let normalized = normalize_loose_bullets(text);
    let text = normalized.as_ref();
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let mut visitor = Visitor::new();
    for event in Parser::new_ext(text, opts) {
        visitor.handle(event);
    }
    let nodes = visitor.finish();
    let mut out = Vec::new();
    let mut open: Option<OpenList> = None;
    flatten(&nodes, 0, &mut out, &mut open);
    flush_open(&mut open, &mut out);
    out
}

/// A list run being accumulated at a single (style, indent) during flattening.
struct OpenList {
    ordered: bool,
    indent: usize,
    sections: Vec<Value>,
}

fn flush_open(open: &mut Option<OpenList>, out: &mut Vec<Value>) {
    if let Some(list) = open.take() {
        out.push(json!({
            "type": "rich_text_list",
            "style": if list.ordered { "ordered" } else { "bullet" },
            "indent": list.indent,
            "elements": list.sections,
        }));
    }
}

/// Walk the nested tree, emitting Slack's flat element sequence. Nested lists
/// become sibling `rich_text_list` elements with a higher `indent`; a nested
/// list inside an item closes the current list run (Slack renders the
/// continuation as a fresh bullet run, which is visually seamless).
fn flatten(nodes: &[Node], indent: usize, out: &mut Vec<Value>, open: &mut Option<OpenList>) {
    for node in nodes {
        match node {
            Node::Section(inlines) => {
                flush_open(open, out);
                push_section(inlines, out);
            }
            Node::Heading(inlines, level) => {
                flush_open(open, out);
                push_heading(inlines, *level, out);
            }
            Node::Quote(inlines) => {
                flush_open(open, out);
                if !inlines.is_empty() {
                    out.push(json!({ "type": "rich_text_quote", "elements": inlines }));
                }
            }
            Node::Pre(body) => {
                flush_open(open, out);
                push_preformatted(body, out);
            }
            Node::Rule => {
                flush_open(open, out);
                // A bare `divider` marker in the element stream; `blockify`
                // lifts it out as its own top-level block.
                out.push(json!({ "type": "divider" }));
            }
            Node::List { ordered, items } => {
                for item in items {
                    // The item's leading inline content (its first Section) is
                    // the bullet text; any deeper blocks emit after it.
                    let (lead, rest) = split_leading_section(item);
                    if !matches_open(open, *ordered, indent) {
                        flush_open(open, out);
                        *open = Some(OpenList {
                            ordered: *ordered,
                            indent: indent.min(MAX_LIST_INDENT),
                            sections: Vec::new(),
                        });
                    }
                    if let Some(list) = open.as_mut() {
                        list.sections
                            .push(json!({ "type": "rich_text_section", "elements": lead }));
                    }
                    if !rest.is_empty() {
                        flush_open(open, out);
                        flatten(rest, indent + 1, out, open);
                    }
                }
                flush_open(open, out);
            }
        }
    }
}

fn matches_open(open: &Option<OpenList>, ordered: bool, indent: usize) -> bool {
    matches!(open, Some(l) if l.ordered == ordered && l.indent == indent.min(MAX_LIST_INDENT))
}

/// Split an item's child nodes into (leading-inline-run, remaining-blocks).
/// pulldown wraps an item's text in a Paragraph for loose lists and emits bare
/// inline text for tight lists — the visitor normalises both into a leading
/// `Section`, so the bullet content is that section's inlines.
fn split_leading_section(item: &[Node]) -> (Vec<Value>, &[Node]) {
    match item.split_first() {
        Some((Node::Section(inlines), rest)) => (inlines.to_vec(), rest),
        _ => (Vec::new(), item),
    }
}

fn push_section(inlines: &[Value], out: &mut Vec<Value>) {
    if inlines.is_empty() {
        return;
    }
    for chunk in split_inline_run(inlines) {
        out.push(json!({ "type": "rich_text_section", "elements": chunk }));
    }
}

/// Emit a heading section, tagged with a private `_heading` marker carrying the
/// level so [`blockify`] gives it its own block (and a leading `divider` for
/// top-level headings). The marker is stripped before the block reaches Slack.
fn push_heading(inlines: &[Value], level: u8, out: &mut Vec<Value>) {
    if inlines.is_empty() {
        return;
    }
    out.push(json!({
        "type": "rich_text_section",
        "elements": inlines,
        "_heading": level,
    }));
}

/// pulldown's `HeadingLevel` → a plain 1–6.
fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Heading levels at or above this rank (i.e. `#` and `##`) get a leading
/// `divider`; deeper headings render as a bold section only.
const DIVIDER_HEADING_LEVEL: u64 = 2;

fn push_preformatted(body: &str, out: &mut Vec<Value>) {
    let body = body.trim_end_matches('\n');
    if body.is_empty() {
        return;
    }
    for chunk in split_text(body, TEXT_RUN_LIMIT) {
        out.push(json!({
            "type": "rich_text_preformatted",
            "elements": [{ "type": "text", "text": chunk }],
        }));
    }
}

/// Split an inline run so no single emitted section carries more than
/// `TEXT_RUN_LIMIT` characters of text, keeping each block comfortably under
/// the payload ceiling. Splits only at element boundaries.
fn split_inline_run(inlines: &[Value]) -> Vec<Vec<Value>> {
    let mut chunks: Vec<Vec<Value>> = Vec::new();
    let mut current: Vec<Value> = Vec::new();
    let mut count = 0usize;
    for el in inlines {
        let len = el
            .get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.chars().count())
            .unwrap_or(0);
        if count + len > TEXT_RUN_LIMIT && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            count = 0;
        }
        current.push(el.clone());
        count += len;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(Vec::new());
    }
    chunks
}

fn split_text(text: &str, limit: usize) -> Vec<String> {
    if text.chars().count() <= limit {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.chars().count() <= limit {
            out.push(remaining.to_string());
            break;
        }
        let cut = remaining
            .char_indices()
            .nth(limit)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        let window = &remaining[..cut];
        let split_at = window.rfind('\n').filter(|i| *i > 0).unwrap_or(cut);
        out.push(remaining[..split_at].to_string());
        remaining = remaining[split_at..].trim_start_matches('\n');
    }
    out
}

struct Block {
    kind: Kind,
    inlines: Vec<Value>,
    children: Vec<Node>,
    items: Vec<Vec<Node>>,
}

#[derive(PartialEq)]
enum Kind {
    Root,
    Paragraph,
    /// Auto-opened to hold inline content where a block parent is required
    /// (tight list items). Closed at block-level transitions.
    ImplicitParagraph,
    /// Markdown heading, carrying its level (1–6) so the renderer can give
    /// top-level sections a divider while leaving subsections lighter.
    Heading(u8),
    Quote,
    List {
        ordered: bool,
    },
    Item,
    Code,
}

impl Kind {
    fn accepts_inline(&self) -> bool {
        matches!(
            self,
            Kind::Paragraph | Kind::ImplicitParagraph | Kind::Heading(_) | Kind::Quote | Kind::Code
        )
    }
}

struct Visitor {
    stack: Vec<Block>,
    bold: u32,
    italic: u32,
    strike: u32,
    /// Active markdown-link target and the label being accumulated for it.
    link: Option<(String, String)>,
    code_buf: String,
    /// Buffer for consecutive plain-text events at the current (uniform) style.
    /// pulldown splits a text run at every `<`, so a Slack wire token like
    /// `<@U123>` arrives as several events; we accumulate and scan the whole
    /// run at flush time (style boundaries, inline elements, block transitions)
    /// so tokens reassemble. Always uniform-style because every style change
    /// flushes first.
    pending: String,
    /// Table cells collected while inside a markdown table (rendered to a
    /// preformatted block on close — `rich_text` has no table primitive).
    table: Option<TableState>,
}

struct TableState {
    rows: Vec<Vec<String>>,
    cell: String,
    in_cell: bool,
}

impl Visitor {
    fn new() -> Self {
        Self {
            stack: vec![Block {
                kind: Kind::Root,
                inlines: Vec::new(),
                children: Vec::new(),
                items: Vec::new(),
            }],
            bold: 0,
            italic: 0,
            strike: 0,
            link: None,
            code_buf: String::new(),
            pending: String::new(),
            table: None,
        }
    }

    fn style(&self) -> Style {
        Style {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            code: false,
        }
    }

    fn handle(&mut self, event: Event) {
        if self.table.is_some() {
            self.handle_table(event);
            return;
        }
        // Flush buffered text before any non-plain-text event so token scanning
        // sees a complete run and ordering / style boundaries are preserved.
        // Text-like events (including raw HTML fragments, which is how pulldown
        // surfaces the `<` of a `<@U123>` token) must NOT flush — that would
        // split a token mid-run.
        if !matches!(
            event,
            Event::Text(_)
                | Event::SoftBreak
                | Event::HardBreak
                | Event::Html(_)
                | Event::InlineHtml(_)
        ) {
            self.flush_pending();
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(end) => self.end(end),
            Event::Text(t) => self.text(t.as_ref()),
            Event::Code(t) => self.inline_code(t.as_ref()),
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.text("\n"),
            // A thematic break is block-level: close any open implicit paragraph
            // and record a `Rule` node, which `flatten`/`blockify` turn into a
            // top-level `divider` block (rich_text has no inline rule primitive).
            Event::Rule => {
                self.close_implicit();
                self.attach_node(Node::Rule);
            }
            Event::Html(t) | Event::InlineHtml(t) => self.text(t.as_ref()),
            Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        let inline = matches!(
            &tag,
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } | Tag::Image { .. }
        );
        if !inline {
            self.close_implicit();
        }
        match tag {
            Tag::Paragraph => self.push(Kind::Paragraph),
            Tag::Heading { level, .. } => self.push(Kind::Heading(heading_level(level))),
            Tag::BlockQuote(_) => self.push(Kind::Quote),
            // Language hint (`Fenced(lang)`) is intentionally dropped — Slack's
            // preformatted element has no syntax-highlight field.
            Tag::CodeBlock(_) => {
                self.code_buf.clear();
                self.push(Kind::Code);
            }
            Tag::List(start) => self.push(Kind::List {
                ordered: start.is_some(),
            }),
            Tag::Item => self.push(Kind::Item),
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.link = Some((dest_url.to_string(), String::new()));
            }
            Tag::Table(_) => {
                self.table = Some(TableState {
                    rows: Vec::new(),
                    cell: String::new(),
                    in_cell: false,
                });
            }
            _ => {}
        }
    }

    fn end(&mut self, end: TagEnd) {
        if matches!(end, TagEnd::Item | TagEnd::BlockQuote(_) | TagEnd::List(_)) {
            self.close_implicit();
        }
        match end {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::BlockQuote(_) | TagEnd::CodeBlock => {
                self.pop()
            }
            TagEnd::List(_) | TagEnd::Item => self.pop(),
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link | TagEnd::Image => self.finish_link(),
            _ => {}
        }
    }

    fn finish_link(&mut self) {
        let Some((url, label)) = self.link.take() else {
            return;
        };
        // An angle-bracket autolink `<url|label>` reaches pulldown as a single
        // Link whose `dest_url` carries the pipe; split it back out. A normal
        // `[label](url)` markdown link has no pipe and a real label.
        let (url, alt_label) = match url.split_once('|') {
            Some((u, l)) => (u.to_string(), Some(l.to_string())),
            None => (url, None),
        };
        let text = if !label.is_empty() && !label.contains('|') {
            label
        } else {
            alt_label.unwrap_or_else(|| url.clone())
        };
        self.ensure_inline();
        let node = with_style(
            json!({ "type": "link", "url": url, "text": text }),
            self.style(),
        );
        self.attach_inline(node);
    }

    fn text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if let Some((_, label)) = self.link.as_mut() {
            label.push_str(s);
            return;
        }
        if matches!(self.cur_kind(), Kind::Code) {
            self.code_buf.push_str(s);
            return;
        }
        self.pending.push_str(s);
    }

    /// Emit the buffered plain-text run: scan it for Slack wire tokens and
    /// attach the resulting `text` / `user` / `channel` / `link` / … elements
    /// at the current style. A no-op when the buffer is empty.
    fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending);
        let style = self.style();
        self.ensure_inline();
        for piece in scan_tokens(&text) {
            let node = match piece {
                Piece::Plain(t) => with_style(json!({ "type": "text", "text": t }), style),
                // Style only applies to text and links; structural mention
                // tokens carry no style object.
                Piece::Token(v) if v["type"] == "link" => with_style(v, style),
                Piece::Token(v) => v,
            };
            self.attach_inline(node);
        }
    }

    fn inline_code(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if let Some((_, label)) = self.link.as_mut() {
            label.push_str(s);
            return;
        }
        self.flush_pending();
        self.ensure_inline();
        let mut style = self.style();
        style.code = true;
        self.attach_inline(with_style(json!({ "type": "text", "text": s }), style));
    }

    fn handle_table(&mut self, event: Event) {
        let Some(state) = self.table.as_mut() else {
            return;
        };
        match event {
            Event::Start(Tag::TableCell) => {
                state.in_cell = true;
                state.cell.clear();
            }
            Event::End(TagEnd::TableCell) => {
                state.in_cell = false;
                let cell = std::mem::take(&mut state.cell);
                match state.rows.last_mut() {
                    Some(row) => row.push(cell),
                    None => state.rows.push(vec![cell]),
                }
            }
            Event::Start(Tag::TableRow) | Event::Start(Tag::TableHead) => {
                state.rows.push(Vec::new());
            }
            Event::Text(t) | Event::Code(t) if state.in_cell => state.cell.push_str(t.as_ref()),
            Event::End(TagEnd::Table) => {
                let rows = std::mem::take(&mut state.rows);
                self.table = None;
                self.close_implicit();
                let rendered = render_table(&rows);
                if !rendered.is_empty() {
                    self.attach_node(Node::Pre(rendered));
                }
            }
            _ => {}
        }
    }

    fn ensure_inline(&mut self) {
        if !self.cur_kind().accepts_inline() {
            self.push(Kind::ImplicitParagraph);
        }
    }

    fn close_implicit(&mut self) {
        if matches!(self.cur_kind(), Kind::ImplicitParagraph) {
            self.pop();
        }
    }

    fn cur_kind(&self) -> &Kind {
        &self.stack.last().expect("stack never empty").kind
    }

    fn push(&mut self, kind: Kind) {
        self.stack.push(Block {
            kind,
            inlines: Vec::new(),
            children: Vec::new(),
            items: Vec::new(),
        });
    }

    fn attach_inline(&mut self, node: Value) {
        if let Some(top) = self.stack.last_mut() {
            top.inlines.push(node);
        }
    }

    fn attach_node(&mut self, node: Node) {
        if let Some(top) = self.stack.last_mut() {
            top.children.push(node);
        }
    }

    fn pop(&mut self) {
        if self.stack.len() <= 1 {
            return;
        }
        let block = self.stack.pop().expect("len > 1 checked");
        match block.kind {
            Kind::Paragraph | Kind::ImplicitParagraph => {
                self.attach_node(Node::Section(block.inlines));
            }
            Kind::Heading(level) => {
                let bolded = block.inlines.into_iter().map(force_bold).collect();
                self.attach_node(Node::Heading(bolded, level));
            }
            Kind::Quote => {
                // pulldown wraps quote content in paragraph(s), so the inline
                // runs land in `children` as Sections; gather them. Fall back to
                // any direct inlines for safety.
                let mut inlines: Vec<Value> = block
                    .children
                    .into_iter()
                    .filter_map(|n| match n {
                        Node::Section(v) => Some(v),
                        _ => None,
                    })
                    .flatten()
                    .collect();
                if inlines.is_empty() {
                    inlines = block.inlines;
                }
                self.attach_node(Node::Quote(inlines));
            }
            Kind::Code => {
                let body = std::mem::take(&mut self.code_buf);
                self.attach_node(Node::Pre(body));
            }
            Kind::List { ordered } => self.attach_node(Node::List {
                ordered,
                items: block.items,
            }),
            Kind::Item => {
                // An item's collected children become one entry in the parent
                // list's `items`.
                if let Some(parent) = self.stack.last_mut() {
                    parent.items.push(block.children);
                }
            }
            Kind::Root => {}
        }
    }

    fn finish(mut self) -> Vec<Node> {
        self.flush_pending();
        while self.stack.len() > 1 {
            self.pop();
        }
        self.stack
            .pop()
            .map(|root| root.children)
            .unwrap_or_default()
    }
}

/// Force a `bold` style onto an already-built inline node (used for headings).
/// Only `text` and `link` elements carry a `style`; structural elements such as
/// `emoji` / `user` / `channel` must be left untouched (Slack rejects or
/// ignores a `style` on them).
fn force_bold(node: Value) -> Value {
    let styleable = node
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t == "text" || t == "link");
    if !styleable {
        return node;
    }
    let mut node = node;
    if let Some(obj) = node.as_object_mut() {
        let style = obj
            .entry("style")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(s) = style.as_object_mut() {
            s.insert("bold".into(), Value::Bool(true));
        }
    }
    node
}

/// Render a parsed markdown table to aligned, fixed-width text for a
/// preformatted block. No dashed separator row — those render as a wall of
/// literal `-` in Slack.
fn render_table(rows: &[Vec<String>]) -> String {
    let rows: Vec<&Vec<String>> = rows.iter().filter(|r| !r.is_empty()).collect();
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in rows {
        let line: Vec<String> = (0..cols)
            .map(|i| {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                let pad = widths[i].saturating_sub(cell.chars().count());
                format!("{cell}{}", " ".repeat(pad))
            })
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

// ── Slack-token scanning ─────────────────────────────────────────────────────

enum Piece {
    Plain(String),
    Token(Value),
}

/// Split a text run into plain text and structured Slack-token elements.
///
/// Three passes, outermost first so each only sees text the prior pass left as
/// plain: angle-bracket wire tokens (`<@U…>`, `<#C…|name>`, `<!here>`,
/// `<url|label>`), then bare URLs, then `:emoji:` shortcodes. URLs are pulled
/// out before emoji so a `:port:` or `:path:` colon inside a URL is never
/// mistaken for a shortcode.
fn scan_tokens(text: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    for piece in scan_angle_tokens(text) {
        match piece {
            Piece::Plain(s) => scan_urls(&s, &mut out),
            token => out.push(token),
        }
    }
    out
}

/// Pass 1: split out the `<…>` Slack wire tokens. Anything between `<` and `>`
/// that doesn't match a known shape is left as literal text.
fn scan_angle_tokens(text: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        if let Some(close_rel) = rest[open + 1..].find('>') {
            let close = open + 1 + close_rel;
            let inner = &rest[open + 1..close];
            if let Some(token) = token_element(inner) {
                if open > 0 {
                    out.push(Piece::Plain(rest[..open].to_string()));
                }
                out.push(Piece::Token(token));
                rest = &rest[close + 1..];
                continue;
            }
        }
        // No closing `>` or unrecognised token: keep the `<` as literal text
        // and advance past it so we don't loop.
        out.push(Piece::Plain(rest[..=open].to_string()));
        rest = &rest[open + 1..];
    }
    if !rest.is_empty() {
        out.push(Piece::Plain(rest.to_string()));
    }
    out
}

/// URL schemes we autolink, longest-prefix first so `https://` wins over a
/// (non-existent) `http://` substring of it.
const URL_SCHEMES: &[&str] = &["https://", "http://", "mailto:"];

/// Pass 2: pull bare URLs out of a plain run into `link` elements, scanning the
/// gaps between them for emoji. A scheme only starts a URL at a word boundary
/// (start-of-run or after a non-alphanumeric char) so `shttp://…` isn't matched
/// mid-word.
fn scan_urls(text: &str, out: &mut Vec<Piece>) {
    let mut rest = text;
    loop {
        let hit = URL_SCHEMES
            .iter()
            .filter_map(|s| rest.find(s).map(|i| (i, *s)))
            .filter(|(i, _)| *i == 0 || !at_alphanumeric(rest, *i - 1))
            .min_by_key(|(i, _)| *i);
        match hit {
            Some((idx, _)) => {
                let len = url_len(&rest[idx..]);
                if len == 0 {
                    // Scheme with no body (e.g. trailing "https://"): treat the
                    // scheme char as plain and continue past it.
                    scan_emoji(&rest[..=idx], out);
                    rest = &rest[idx + 1..];
                    continue;
                }
                if idx > 0 {
                    scan_emoji(&rest[..idx], out);
                }
                let url = &rest[idx..idx + len];
                out.push(Piece::Token(
                    json!({ "type": "link", "url": url, "text": url }),
                ));
                rest = &rest[idx + len..];
            }
            None => {
                scan_emoji(rest, out);
                break;
            }
        }
    }
}

/// True if the byte ending at `text[..end]` is an ASCII alphanumeric.
fn at_alphanumeric(text: &str, end: usize) -> bool {
    text[..end + 1]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric())
}

/// Length (bytes) of the URL starting at the front of `s`. Consumes up to the
/// first whitespace / delimiter, then trims trailing sentence punctuation that
/// is almost never part of a URL. A trailing `)` is kept only when the URL has
/// an unmatched `(` (e.g. Wikipedia paths).
fn url_len(s: &str) -> usize {
    let mut end = s.len();
    for (i, c) in s.char_indices() {
        if c.is_whitespace() || matches!(c, '<' | '>' | '|' | '`' | '"') {
            end = i;
            break;
        }
    }
    let has_open_paren = s[..end].contains('(');
    while end > 0 {
        let last = s[..end].chars().next_back().expect("end > 0");
        let trim = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | ']' | '}' | '\'' => true,
            ')' => !has_open_paren,
            _ => false,
        };
        if trim {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    // A bare scheme with nothing after it isn't a link.
    if URL_SCHEMES.iter().any(|sch| s[..end] == **sch) {
        return 0;
    }
    end
}

/// Pass 3: split `:shortcode:` emoji out of a (URL-free) plain run into `emoji`
/// elements, coalescing the surrounding text so we don't fragment it into many
/// tiny `text` runs (timestamps like `00:32:08` are full of colons).
fn scan_emoji(text: &str, out: &mut Vec<Piece>) {
    let mut buf = String::new();
    let mut rest = text;
    while let Some(open) = rest.find(':') {
        // The opening `:` must sit on a word boundary (start of the run, or
        // after a non-alphanumeric char). Real shortcodes are written that way
        // (`see :mag:`), while technical strings like `host:port:path` have the
        // colon glued to a letter — so we leave those untouched.
        let prev = if open > 0 {
            rest[..open].chars().next_back()
        } else {
            buf.chars().next_back()
        };
        let boundary_ok = prev.is_none_or(|c| !c.is_alphanumeric());
        if boundary_ok {
            if let Some((name, consumed)) = match_shortcode(&rest[open..]) {
                buf.push_str(&rest[..open]);
                if !buf.is_empty() {
                    out.push(Piece::Plain(std::mem::take(&mut buf)));
                }
                out.push(Piece::Token(json!({ "type": "emoji", "name": name })));
                rest = &rest[open + consumed..];
                continue;
            }
        }
        // Not a shortcode (or not on a boundary): keep through this colon and
        // advance past it so a stray `:` can't loop.
        buf.push_str(&rest[..=open]);
        rest = &rest[open + 1..];
    }
    buf.push_str(rest);
    if !buf.is_empty() {
        out.push(Piece::Plain(buf));
    }
}

/// If `s` (which starts with `:`) opens a valid `:name:` shortcode, return the
/// bare name and the byte length consumed (including both colons).
fn match_shortcode(s: &str) -> Option<(String, usize)> {
    let close = s[1..].find(':')? + 1;
    let name = &s[1..close];
    is_valid_emoji_name(name).then(|| (name.to_string(), close + 1))
}

/// Slack emoji names are lowercase `[a-z0-9_+-]`. We additionally require a
/// leading letter — or the two all-symbol names `+1` / `-1` — so digit-only
/// runs from timestamps (`:32:`, `:08:`) and IDs never resolve as emoji.
fn is_valid_emoji_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 100 {
        return false;
    }
    let valid_chars = name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'+' | b'-'));
    if !valid_chars {
        return false;
    }
    name.as_bytes()[0].is_ascii_lowercase() || name == "+1" || name == "-1"
}

fn token_element(inner: &str) -> Option<Value> {
    let (head, label) = match inner.split_once('|') {
        Some((h, l)) => (h, Some(l)),
        None => (inner, None),
    };
    let mut chars = head.chars();
    match chars.next()? {
        '@' => {
            let id = &head[1..];
            (!id.is_empty()).then(|| json!({ "type": "user", "user_id": id }))
        }
        '#' => {
            let id = &head[1..];
            (!id.is_empty()).then(|| json!({ "type": "channel", "channel_id": id }))
        }
        '!' => broadcast_element(&head[1..], label),
        _ if is_url(head) => Some(json!({
            "type": "link",
            "url": head,
            "text": label.filter(|l| !l.is_empty()).unwrap_or(head),
        })),
        _ => None,
    }
}

fn broadcast_element(body: &str, label: Option<&str>) -> Option<Value> {
    if matches!(body, "here" | "channel" | "everyone") {
        return Some(json!({ "type": "broadcast", "range": body }));
    }
    if let Some(id) = body.strip_prefix("subteam^") {
        let id = id.split('^').next().unwrap_or(id);
        return Some(json!({ "type": "usergroup", "usergroup_id": id }));
    }
    // `<!date^…>` and other specials: fall back to their label as plain text.
    label.map(|l| json!({ "type": "text", "text": l }))
}

fn is_url(s: &str) -> bool {
    s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("mailto:")
        || s.starts_with("tel:")
}

// ── Block packing & message grouping ─────────────────────────────────────────

/// Turn the flat `rich_text` element stream into Block Kit blocks, choosing
/// block boundaries for readability rather than packing everything into one
/// dense `rich_text` block (the old behaviour, where every paragraph stacked
/// with no vertical gap).
///
/// - Each prose paragraph / quote / code block becomes its **own** `rich_text`
///   block. Slack renders a consistent margin *between* top-level blocks but
///   none between sibling sections inside one block, so this is what gives
///   paragraphs breathing room.
/// - Consecutive list elements (a list and its nested sub-lists) are kept
///   together in a single block so the nesting renders as one continuous list.
/// - A heading gets its own block and, unless it's the very first block, a
///   `divider` in front of it — turning a wall of text into labelled sections.
/// - A `divider` element (from a markdown thematic break, `---` / `***`) is
///   lifted out as its own top-level block, deduped against an adjacent
///   heading-divider and stripped if it would lead or trail the message.
fn blockify(elements: Vec<Value>) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    let mut list_run: Vec<Value> = Vec::new();
    for mut el in elements {
        let kind = el.get("type").and_then(Value::as_str);
        if kind == Some("rich_text_list") {
            list_run.push(el);
            continue;
        }
        if kind == Some("divider") {
            flush_list_run(&mut list_run, &mut blocks);
            // Skip a leading divider, and never stack two in a row (e.g. a `---`
            // immediately before a top-level heading that also wants one).
            if !blocks.is_empty() && !last_is_divider(&blocks) {
                blocks.push(el);
            }
            continue;
        }
        flush_list_run(&mut list_run, &mut blocks);
        let heading_level = el
            .as_object_mut()
            .and_then(|o| o.remove("_heading"))
            .and_then(|v| v.as_u64());
        if let Some(level) = heading_level {
            // Top-level sections get a divider; subsections stay light so the
            // message doesn't turn into a wall of horizontal rules.
            if level <= DIVIDER_HEADING_LEVEL && !blocks.is_empty() && !last_is_divider(&blocks) {
                blocks.push(json!({ "type": "divider" }));
            }
        }
        push_rich_text_block(std::slice::from_ref(&el), &mut blocks);
    }
    flush_list_run(&mut list_run, &mut blocks);
    // A divider dangling at the very end (a trailing `---`) renders as an empty
    // rule under the last section; drop it.
    if last_is_divider(&blocks) {
        blocks.pop();
    }
    blocks
}

/// Whether `block` is a `divider`.
fn is_divider(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("divider")
}

/// Whether the last block already emitted is a `divider`.
fn last_is_divider(blocks: &[Value]) -> bool {
    blocks.last().is_some_and(is_divider)
}

/// A blank-line spacer: a `rich_text` block whose only content is a single
/// non-breaking space. Slack renders it as one visually empty line, turning the
/// thin inter-block margin into a clear gap between paragraphs and sections.
fn spacer_block() -> Value {
    json!({
        "type": "rich_text",
        "elements": [{
            "type": "rich_text_section",
            "elements": [{ "type": "text", "text": "\u{00A0}" }],
        }],
    })
}

/// Whether `block` is a [`spacer_block`].
fn is_spacer(block: &Value) -> bool {
    *block == spacer_block()
}

/// Insert a [`spacer_block`] between adjacent top-level blocks so paragraphs and
/// sections render with a blank line between them. Skipped where either
/// neighbour is a `divider` (which already separates), and never leading or
/// trailing.
fn space_blocks(blocks: Vec<Value>) -> Vec<Value> {
    let mut spaced: Vec<Value> = Vec::with_capacity(blocks.len().saturating_mul(2));
    for block in blocks {
        if spaced
            .last()
            .is_some_and(|prev| !is_divider(prev) && !is_divider(&block))
        {
            spaced.push(spacer_block());
        }
        spaced.push(block);
    }
    spaced
}

/// Emit any accumulated list elements as a single `rich_text` block (chunked
/// only if they somehow exceed the per-block element cap).
fn flush_list_run(list_run: &mut Vec<Value>, blocks: &mut Vec<Value>) {
    if list_run.is_empty() {
        return;
    }
    let run = std::mem::take(list_run);
    push_rich_text_block(&run, blocks);
}

/// Wrap `elements` in one or more `rich_text` blocks, splitting only if the
/// element count would exceed [`MAX_RICH_TEXT_ELEMENTS`].
fn push_rich_text_block(elements: &[Value], blocks: &mut Vec<Value>) {
    for chunk in elements.chunks(MAX_RICH_TEXT_ELEMENTS) {
        if chunk.is_empty() {
            continue;
        }
        blocks.push(json!({ "type": "rich_text", "elements": chunk.to_vec() }));
    }
}

fn group_into_messages(blocks: Vec<Value>, paragraph_spacing: bool) -> Vec<Vec<Value>> {
    if blocks.is_empty() {
        return Vec::new();
    }
    let mut groups: Vec<Vec<Value>> = Vec::new();
    let mut current: Vec<Value> = Vec::new();
    let mut bytes = MSG_OVERHEAD_BYTES;
    for block in blocks {
        let size = serde_json::to_vec(&block).map(|v| v.len()).unwrap_or(0);
        let over_payload = bytes + size > PAYLOAD_BUDGET_BYTES;
        let over_blocks = current.len() >= MAX_BLOCKS_PER_MESSAGE;
        if (over_payload || over_blocks) && !current.is_empty() {
            groups.push(std::mem::take(&mut current));
            bytes = MSG_OVERHEAD_BYTES;
        }
        // A spacer that would lead a continuation message renders as a stray
        // blank line at its top; skip it.
        if paragraph_spacing && current.is_empty() && is_spacer(&block) {
            continue;
        }
        bytes += size;
        current.push(block);
    }
    if !current.is_empty() {
        groups.push(current);
    }
    // Likewise drop a spacer left dangling at the end of a group by a split.
    if paragraph_spacing {
        for group in &mut groups {
            if group.last().is_some_and(is_spacer) {
                group.pop();
            }
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect all top-level `rich_text` element objects across the messages.
    fn elements(text: &str) -> Vec<Value> {
        build_messages(text, false)
            .into_iter()
            .flatten()
            .flat_map(|block| block["elements"].as_array().cloned().unwrap_or_default())
            .collect()
    }

    /// Collect the Block Kit blocks across all message groups (their `type`s
    /// reveal layout: `rich_text` / `divider`).
    fn blocks(text: &str) -> Vec<Value> {
        build_messages(text, false).into_iter().flatten().collect()
    }

    /// Like [`blocks`] but with paragraph spacing on.
    fn spaced_blocks(text: &str) -> Vec<Value> {
        build_messages(text, true).into_iter().flatten().collect()
    }

    fn block_types(text: &str) -> Vec<String> {
        blocks(text)
            .iter()
            .map(|b| b["type"].as_str().unwrap_or("").to_string())
            .collect()
    }

    fn first_of_type<'a>(els: &'a [Value], ty: &str) -> &'a Value {
        els.iter()
            .find(|e| e["type"] == ty)
            .unwrap_or_else(|| panic!("no element of type {ty} in {els:#?}"))
    }

    /// Concatenate every `text` field found anywhere in a value tree.
    fn all_text(v: &Value) -> String {
        let mut out = String::new();
        collect_text(v, &mut out);
        out
    }
    fn collect_text(v: &Value, out: &mut String) {
        match v {
            Value::Object(m) => {
                if let (Some(Value::String(t)), Some(Value::String(ty))) =
                    (m.get("text"), m.get("type"))
                {
                    if ty == "text" {
                        out.push_str(t);
                    }
                }
                for (_, val) in m {
                    collect_text(val, out);
                }
            }
            Value::Array(a) => a.iter().for_each(|x| collect_text(x, out)),
            _ => {}
        }
    }

    #[test]
    fn empty_input_yields_no_messages() {
        assert!(build_messages("", false).is_empty());
        assert!(build_messages("   \n  ", false).is_empty());
    }

    #[test]
    fn plain_paragraph_is_a_section() {
        let els = elements("hello world");
        assert_eq!(els.len(), 1);
        assert_eq!(els[0]["type"], "rich_text_section");
        assert_eq!(els[0]["elements"][0]["type"], "text");
        assert_eq!(els[0]["elements"][0]["text"], "hello world");
        // Plain text carries no style object.
        assert!(els[0]["elements"][0].get("style").is_none());
    }

    #[test]
    fn bold_italic_strike_styles() {
        let els = elements("**b** _i_ ~~s~~");
        let sec = &els[0]["elements"];
        let bold = sec
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["text"] == "b")
            .unwrap();
        assert_eq!(bold["style"]["bold"], true);
        let ital = sec
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["text"] == "i")
            .unwrap();
        assert_eq!(ital["style"]["italic"], true);
        let strike = sec
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["text"] == "s")
            .unwrap();
        assert_eq!(strike["style"]["strike"], true);
    }

    #[test]
    fn inline_code_style() {
        let els = elements("use `cargo test` now");
        let code = els[0]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["text"] == "cargo test")
            .unwrap();
        assert_eq!(code["style"]["code"], true);
    }

    #[test]
    fn markdown_link_becomes_link_element() {
        let els = elements("see [the docs](https://example.com) please");
        let link = first_of_type(els[0]["elements"].as_array().unwrap(), "link");
        assert_eq!(link["url"], "https://example.com");
        assert_eq!(link["text"], "the docs");
    }

    #[test]
    fn bullet_list_renders_as_rich_text_list() {
        let els = elements("- one\n- two");
        let list = first_of_type(&els, "rich_text_list");
        assert_eq!(list["style"], "bullet");
        assert_eq!(list["indent"], 0);
        let items = list["elements"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "rich_text_section");
        assert_eq!(all_text(&items[0]), "one");
        assert_eq!(all_text(&items[1]), "two");
    }

    #[test]
    fn star_bullets_do_not_leak_literal_asterisks() {
        // The reported SS2 bug: `* **Label:** text` showed a literal `*`.
        let els = elements("* **Reschedules:** Added Slack alerts\n* **Help:** New form");
        let list = first_of_type(&els, "rich_text_list");
        assert_eq!(list["style"], "bullet");
        let joined = all_text(&els[0]);
        assert!(!joined.contains('*'), "no literal asterisk: {joined:?}");
        let items = list["elements"].as_array().unwrap();
        assert!(all_text(&items[0]).starts_with("Reschedules:"));
        // The label is bold.
        let bold = items[0]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| {
                e["text"]
                    .as_str()
                    .map(|s| s.contains("Reschedules"))
                    .unwrap_or(false)
            })
            .unwrap();
        assert_eq!(bold["style"]["bold"], true);
    }

    #[test]
    fn literal_bullet_glyphs_become_a_real_list() {
        // The reported wall-of-text bug: a report that used the literal `•`
        // glyph (not a markdown `-`/`*` marker) collapsed onto one line because
        // pulldown ignores `•` as a marker and joins the single newlines as
        // soft breaks. Normalisation rewrites `• ` → `- ` so a real list is
        // built — one item per line — and the bold/emoji inside still render.
        let els = elements(
            "*10 unreleased PRs* since the release, broken down into:\n\
             • :inbox_tray: *Job Plan Imports* — new flow\n\
             • :hospital: *Cleveland Clinic* — self-service",
        );
        let list = first_of_type(&els, "rich_text_list");
        assert_eq!(list["style"], "bullet");
        let items = list["elements"].as_array().unwrap();
        assert_eq!(items.len(), 2, "two bullets → two list items: {els:#?}");
        assert!(all_text(&items[0]).contains("Job Plan Imports"));
        assert!(all_text(&items[1]).contains("Cleveland Clinic"));
        // The literal glyph is gone — no run-on `• a • b` line survives.
        let joined: String = els.iter().map(all_text).collect();
        assert!(
            !joined.contains('•'),
            "no literal bullet glyph left: {joined:?}"
        );
    }

    #[test]
    fn nested_literal_bullets_keep_indent() {
        let els = elements("• outer\n  • inner");
        let lists: Vec<&Value> = els
            .iter()
            .filter(|e| e["type"] == "rich_text_list")
            .collect();
        assert_eq!(lists.len(), 2, "nested glyph bullets flatten to two lists");
        assert_eq!(lists[0]["indent"], 0);
        assert_eq!(lists[1]["indent"], 1);
    }

    #[test]
    fn literal_bullets_inside_code_fence_stay_literal() {
        // A `•` inside a fenced code block is content, not a bullet — leave it.
        let els = elements("```\n• not a bullet\n```");
        let pre = first_of_type(&els, "rich_text_preformatted");
        assert!(all_text(pre).contains("• not a bullet"));
        assert!(!els.iter().any(|e| e["type"] == "rich_text_list"));
    }

    #[test]
    fn middot_in_prose_is_not_a_bullet() {
        // A mid-line `·` (no leading-glyph-at-line-start) must stay prose.
        let els = elements("the ratio a·b holds");
        assert!(!els.iter().any(|e| e["type"] == "rich_text_list"));
        assert!(all_text(&els[0]).contains("a·b"));
    }

    #[test]
    fn over_escaped_entities_are_decoded() {
        // Models over-escape `&` (and friends) into HTML entities. rich_text
        // needs no escaping; pulldown_cmark decodes them per CommonMark, so the
        // viewer sees `&`/`<`/`"`, not the raw entity. (Regression guard: the
        // reported `&amp;` was a copied-source artefact, never a render bug.)
        let els = elements("Job Plan &amp; Clinician Imports &lt;v2&gt; &quot;ok&quot;");
        let joined = all_text(&els[0]);
        assert!(
            joined.contains("Job Plan & Clinician Imports"),
            "{joined:?}"
        );
        assert!(joined.contains("<v2>"), "{joined:?}");
        assert!(joined.contains("\"ok\""), "{joined:?}");
        assert!(!joined.contains("&amp;"));
    }

    #[test]
    fn ordered_list_style() {
        let els = elements("1. first\n2. second");
        let list = first_of_type(&els, "rich_text_list");
        assert_eq!(list["style"], "ordered");
        assert_eq!(list["elements"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn nested_list_uses_indent_levels() {
        let els = elements("- outer\n  - inner");
        let lists: Vec<&Value> = els
            .iter()
            .filter(|e| e["type"] == "rich_text_list")
            .collect();
        assert_eq!(lists.len(), 2, "nested list flattens to two list elements");
        assert_eq!(lists[0]["indent"], 0);
        assert_eq!(all_text(&lists[0]["elements"]), "outer");
        assert_eq!(lists[1]["indent"], 1);
        assert_eq!(all_text(&lists[1]["elements"]), "inner");
    }

    #[test]
    fn heading_becomes_bold_section() {
        let els = elements("## Status");
        assert_eq!(els[0]["type"], "rich_text_section");
        assert_eq!(els[0]["elements"][0]["text"], "Status");
        assert_eq!(els[0]["elements"][0]["style"]["bold"], true);
    }

    #[test]
    fn fenced_code_becomes_preformatted_without_fence_or_lang() {
        let els = elements("```sql\nSELECT 1\n```");
        let pre = first_of_type(&els, "rich_text_preformatted");
        assert_eq!(pre["elements"][0]["text"], "SELECT 1");
        assert!(!all_text(pre).contains("```"));
        assert!(!all_text(pre).contains("sql"));
    }

    #[test]
    fn mixed_prose_and_code_keep_order() {
        let els = elements("intro\n\n```\ncode\n```\n\noutro");
        let types: Vec<&str> = els.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            vec![
                "rich_text_section",
                "rich_text_preformatted",
                "rich_text_section"
            ]
        );
    }

    #[test]
    fn blockquote_becomes_quote() {
        let els = elements("> quoted text");
        let q = first_of_type(&els, "rich_text_quote");
        assert_eq!(all_text(q), "quoted text");
    }

    #[test]
    fn user_token_becomes_structured_user() {
        let els = elements("hi <@U123> there");
        let user = first_of_type(els[0]["elements"].as_array().unwrap(), "user");
        assert_eq!(user["user_id"], "U123");
    }

    #[test]
    fn channel_token_becomes_structured_channel() {
        let els = elements("see <#C999|general> now");
        let ch = first_of_type(els[0]["elements"].as_array().unwrap(), "channel");
        assert_eq!(ch["channel_id"], "C999");
    }

    #[test]
    fn broadcast_token_becomes_broadcast() {
        let els = elements("<!here> ping");
        let b = first_of_type(els[0]["elements"].as_array().unwrap(), "broadcast");
        assert_eq!(b["range"], "here");
    }

    #[test]
    fn explicit_slack_link_token_becomes_link() {
        let els = elements("track it <https://linear.app/x/OPS-438|OPS-438>");
        let link = first_of_type(els[0]["elements"].as_array().unwrap(), "link");
        assert_eq!(link["url"], "https://linear.app/x/OPS-438");
        assert_eq!(link["text"], "OPS-438");
    }

    #[test]
    fn unknown_angle_bracket_stays_literal() {
        let els = elements("a < b and c > d");
        assert_eq!(all_text(&els[0]), "a < b and c > d");
    }

    #[test]
    fn table_renders_without_dashed_separator() {
        let els = elements("| Name | Age |\n|---|---|\n| Alice | 30 |");
        let pre = first_of_type(&els, "rich_text_preformatted");
        let body = all_text(pre);
        assert!(body.contains("Name"));
        assert!(body.contains("Alice"));
        // The hallmark of the old bug: a row of dashes.
        assert!(!body.contains("---"), "no dashed rule: {body:?}");
    }

    #[test]
    fn link_text_inside_list_item() {
        let els = elements("- see [docs](https://x.dev)");
        let list = first_of_type(&els, "rich_text_list");
        let link = first_of_type(list["elements"][0]["elements"].as_array().unwrap(), "link");
        assert_eq!(link["url"], "https://x.dev");
    }

    #[test]
    fn each_message_within_block_limit() {
        let mut input = String::new();
        for i in 0..130 {
            input.push_str(&format!("paragraph number {i}\n\n"));
        }
        for group in build_messages(&input, true) {
            assert!(group.len() <= MAX_BLOCKS_PER_MESSAGE);
        }
    }

    #[test]
    fn rich_text_blocks_respect_element_cap() {
        let mut input = String::new();
        for i in 0..120 {
            input.push_str(&format!("para {i}\n\n"));
        }
        for group in build_messages(&input, true) {
            for block in group {
                let n = block["elements"].as_array().unwrap().len();
                assert!(n <= MAX_RICH_TEXT_ELEMENTS, "block had {n} elements");
            }
        }
    }

    #[test]
    fn paragraph_spacing_inserts_spacer_between_blocks() {
        let blks = spaced_blocks("First paragraph.\n\nSecond paragraph.");
        assert_eq!(blks.len(), 3);
        assert_eq!(blks[0]["type"], "rich_text");
        assert!(is_spacer(&blks[1]));
        assert_eq!(blks[2]["type"], "rich_text");
    }

    #[test]
    fn paragraph_spacing_off_leaves_blocks_untouched() {
        let blks = blocks("First paragraph.\n\nSecond paragraph.");
        assert_eq!(blks.len(), 2);
        assert!(blks.iter().all(|b| !is_spacer(b)));
    }

    #[test]
    fn spacer_separates_paragraph_and_list() {
        let blks = spaced_blocks("Here is what I found:\n\n- one\n- two");
        assert_eq!(blks.len(), 3);
        assert!(is_spacer(&blks[1]));
        assert_eq!(blks[2]["elements"][0]["type"], "rich_text_list");
    }

    #[test]
    fn no_spacer_is_placed_next_to_a_divider() {
        // `intro`, then a `##` heading (which gets a leading divider), then `body`.
        let blks = spaced_blocks("intro\n\n## Section\n\nbody");
        assert_eq!(blks.len(), 5);
        assert!(!is_spacer(&blks[0])); // intro
        assert_eq!(blks[1]["type"], "divider");
        assert!(!is_spacer(&blks[2])); // heading
        assert!(is_spacer(&blks[3])); // the sole spacer: heading ↔ body
        assert!(!is_spacer(&blks[4])); // body
    }

    #[test]
    fn single_block_message_has_no_spacer() {
        let blks = spaced_blocks("just one paragraph");
        assert_eq!(blks.len(), 1);
        assert!(!is_spacer(&blks[0]));
    }

    #[test]
    fn split_groups_never_lead_or_trail_with_a_spacer() {
        let input = (0..40)
            .map(|i| format!("Paragraph number {i}."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let groups = build_messages(&input, true);
        assert!(groups.len() > 1, "expected a multi-message split");
        for group in &groups {
            assert!(!is_spacer(group.first().unwrap()));
            assert!(!is_spacer(group.last().unwrap()));
        }
    }

    #[test]
    fn notification_fallback_strips_code() {
        let fb = notification_fallback("Here you go:\n```sql\nSELECT 1\n```\nDone.");
        assert!(!fb.contains("SELECT"));
        assert!(fb.contains("Here you go"));
        assert!(fb.contains("Done."));
    }

    /// Collect every `emoji` element name found anywhere in the tree.
    fn emoji_names(text: &str) -> Vec<String> {
        fn walk(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::Object(m) => {
                    if m.get("type").and_then(Value::as_str) == Some("emoji") {
                        if let Some(n) = m.get("name").and_then(Value::as_str) {
                            out.push(n.to_string());
                        }
                    }
                    m.values().for_each(|x| walk(x, out));
                }
                Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        for block in build_messages(text, false).into_iter().flatten() {
            walk(&block, &mut out);
        }
        out
    }

    #[test]
    fn emoji_shortcode_becomes_emoji_element() {
        let els = elements(":mag: searching now");
        let emoji = first_of_type(els[0]["elements"].as_array().unwrap(), "emoji");
        assert_eq!(emoji["name"], "mag");
        // The literal `:mag:` is gone from the text runs.
        assert!(!all_text(&els[0]).contains(":mag:"));
        assert!(all_text(&els[0]).contains("searching now"));
    }

    #[test]
    fn plus_one_minus_one_shortcodes_resolve() {
        assert_eq!(emoji_names("nice :+1: but :-1: on that"), vec!["+1", "-1"]);
    }

    #[test]
    fn timestamps_are_never_mistaken_for_emoji() {
        // The screenshot is saturated with these — a naive `:..:` scan would
        // eat `:32:` / `:00:` / `:08:` and corrupt the timeline.
        let text = "Cancellation processed at 00:32 UTC 2026-05-29 00:32:08, order closed 00:00.";
        assert!(emoji_names(text).is_empty(), "no emoji from timestamps");
        let joined = all_text(&elements(text)[0]);
        assert!(joined.contains("00:32:08"), "timestamp intact: {joined:?}");
        assert!(joined.contains("00:00"), "timestamp intact: {joined:?}");
    }

    #[test]
    fn screenshot_fixture_mixes_emoji_and_timestamps() {
        // A reduced version of the reported message: shortcode headings the
        // model emits (`:mag:` / `:dart:`) must render, while the dense
        // timestamps around them must survive untouched.
        let text = ":mag: Full Timeline — Order 1234567\n\n\
                    1. Order created 2026-05-28 11:56, status available.\n\
                    4. Cancellation processed at 00:32 UTC 2026-05-29 00:32:08.\n\n\
                    :dart: Answer to Ana's question 6/7";
        let names = emoji_names(text);
        assert!(names.contains(&"mag".to_string()), "got {names:?}");
        assert!(names.contains(&"dart".to_string()), "got {names:?}");
        assert!(
            !names.iter().any(|n| n.bytes().all(|b| b.is_ascii_digit())),
            "no digit-only emoji from timestamps: {names:?}"
        );
        let joined: String = elements(text).iter().map(all_text).collect();
        assert!(joined.contains("00:32:08"));
        assert!(joined.contains("2026-05-28 11:56"));
        assert!(!joined.contains(":mag:"));
        assert!(!joined.contains(":dart:"));
    }

    #[test]
    fn emoji_inside_inline_code_stays_literal() {
        // Inline code bypasses the scanner, so `:tada:` in `code` is verbatim.
        let els = elements("run `echo :tada:` please");
        assert!(emoji_names("run `echo :tada:` please").is_empty());
        assert!(all_text(&els[0]).contains(":tada:"));
    }

    #[test]
    fn bare_url_becomes_link_element() {
        let els = elements("see https://example.com/path for details");
        let link = first_of_type(els[0]["elements"].as_array().unwrap(), "link");
        assert_eq!(link["url"], "https://example.com/path");
    }

    #[test]
    fn bare_url_trailing_punctuation_trimmed() {
        let els = elements("docs at https://example.com/page, then stop.");
        let link = first_of_type(els[0]["elements"].as_array().unwrap(), "link");
        assert_eq!(link["url"], "https://example.com/page");
        // The comma stays in the prose, not the URL.
        assert!(all_text(&els[0]).contains(", then stop."));
    }

    #[test]
    fn url_with_balanced_parens_keeps_closing_paren() {
        let els = elements("see https://en.wikipedia.org/wiki/Rust_(programming_language) ok");
        let link = first_of_type(els[0]["elements"].as_array().unwrap(), "link");
        assert_eq!(
            link["url"],
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
    }

    #[test]
    fn colon_in_url_not_treated_as_emoji() {
        let text = "ping https://host.local:8080/a:b:c now";
        assert!(emoji_names(text).is_empty(), "no emoji from URL colons");
        let els = elements(text);
        let link = first_of_type(els[0]["elements"].as_array().unwrap(), "link");
        assert_eq!(link["url"], "https://host.local:8080/a:b:c");
    }

    #[test]
    fn emoji_in_heading_carries_no_style() {
        // A `:dart:` in a `## heading` must stay a bare emoji element — Slack
        // rejects/ignores a `style` object on emoji, so heading-bolding must
        // skip it while still bolding the heading text.
        let b = blocks("## :dart: Goals\n\nbody");
        let section = &b[0]["elements"][0];
        let emoji = first_of_type(section["elements"].as_array().unwrap(), "emoji");
        assert_eq!(emoji["name"], "dart");
        assert!(
            emoji.get("style").is_none(),
            "emoji must not be styled: {emoji}"
        );
        // The heading words are still bold.
        let word = section["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["text"].as_str().is_some_and(|s| s.contains("Goals")))
            .unwrap();
        assert_eq!(word["style"]["bold"], true);
    }

    #[test]
    fn each_paragraph_is_its_own_block() {
        // The core layout fix: consecutive paragraphs no longer stack inside a
        // single dense rich_text block.
        let types = block_types("First paragraph.\n\nSecond paragraph.\n\nThird.");
        assert_eq!(types, vec!["rich_text", "rich_text", "rich_text"]);
    }

    #[test]
    fn heading_gets_divider_and_its_own_block() {
        // Lead paragraph, then two headed sections. A divider precedes each
        // heading except when it's the first block.
        let text = "Intro line.\n\n## Timeline\n\nStep one.\n\n## Root cause\n\nBecause.";
        let types = block_types(text);
        assert_eq!(
            types,
            vec![
                "rich_text", // Intro line.
                "divider",   // before ## Timeline
                "rich_text", // Timeline heading
                "rich_text", // Step one.
                "divider",   // before ## Root cause
                "rich_text", // Root cause heading
                "rich_text", // Because.
            ]
        );
    }

    #[test]
    fn subsection_heading_gets_no_divider() {
        // `###`+ headings render bold but without a divider, so deep nesting
        // doesn't become a wall of horizontal rules.
        let text = "## Major\n\nlead\n\n### Minor\n\nbody";
        let types = block_types(text);
        assert_eq!(types.iter().filter(|t| *t == "divider").count(), 0);
        // Both headings still render (as bold sections).
        assert!(text_of_heading(&blocks(text), "Major"));
        assert!(text_of_heading(&blocks(text), "Minor"));
    }

    /// True if some block holds a bold section whose text contains `needle`.
    fn text_of_heading(blocks: &[Value], needle: &str) -> bool {
        blocks.iter().any(|b| {
            all_text(b).contains(needle)
                && b["elements"][0]["elements"][0]["style"]["bold"] == Value::Bool(true)
        })
    }

    #[test]
    fn host_port_path_is_not_emoji() {
        // The colon-in-the-middle-of-a-word case must not match `:port:`.
        let text = "connect to host:port:path now";
        assert!(emoji_names(text).is_empty(), "got {:?}", emoji_names(text));
        assert!(all_text(&elements(text)[0]).contains("host:port:path"));
    }

    #[test]
    fn shortcode_after_punctuation_still_resolves() {
        // Boundary check allows non-alphanumeric predecessors like `(`.
        assert_eq!(emoji_names("ship it (:shipit:)"), vec!["shipit"]);
    }

    #[test]
    fn leading_heading_has_no_divider_before_it() {
        let text = "# Title\n\nBody.";
        let types = block_types(text);
        assert_eq!(types.first().map(String::as_str), Some("rich_text"));
        assert!(!types.contains(&"divider".to_string()));
    }

    #[test]
    fn thematic_break_becomes_divider() {
        // A `---` between sections (the agent's natural "put a divider here")
        // lifts out as its own top-level block — previously it was dropped.
        let types = block_types("Section one.\n\n---\n\nSection two.");
        assert_eq!(types, vec!["rich_text", "divider", "rich_text"]);
    }

    #[test]
    fn asterisk_and_underscore_rules_also_divide() {
        // pulldown treats `***` and `___` as thematic breaks too.
        assert_eq!(
            block_types("a\n\n***\n\nb"),
            vec!["rich_text", "divider", "rich_text"]
        );
        assert_eq!(
            block_types("a\n\n___\n\nb"),
            vec!["rich_text", "divider", "rich_text"]
        );
    }

    #[test]
    fn leading_and_trailing_rules_are_dropped() {
        // A divider with nothing before or after it would render as an empty
        // rule; strip both ends.
        assert_eq!(block_types("---\n\nbody\n\n---"), vec!["rich_text"]);
    }

    #[test]
    fn consecutive_rules_collapse_to_one() {
        let types = block_types("a\n\n---\n\n---\n\nb");
        assert_eq!(types.iter().filter(|t| *t == "divider").count(), 1);
        assert_eq!(types, vec!["rich_text", "divider", "rich_text"]);
    }

    #[test]
    fn rule_before_heading_does_not_double_divider() {
        // A `---` immediately before a `##` heading (which also wants a leading
        // divider) yields a single divider, not two stacked.
        let types = block_types("a\n\n---\n\n## H\n\nb");
        assert_eq!(
            types,
            vec!["rich_text", "divider", "rich_text", "rich_text"]
        );
    }

    #[test]
    fn divider_separates_two_lists() {
        // Lists flush before the divider, so two bullet lists stay distinct
        // blocks with a rule between them rather than merging into one run.
        let types = block_types("- a\n- b\n\n---\n\n- c\n- d");
        assert_eq!(types, vec!["rich_text", "divider", "rich_text"]);
    }

    #[test]
    fn heading_text_is_bold_and_marker_stripped() {
        let b = blocks("## Status\n\nbody");
        let heading = &b[0];
        assert_eq!(heading["type"], "rich_text");
        let section = &heading["elements"][0];
        assert_eq!(section["type"], "rich_text_section");
        // The private layout marker never reaches Slack.
        assert!(section.get("_heading").is_none());
        assert_eq!(section["elements"][0]["style"]["bold"], true);
    }

    #[test]
    fn nested_list_stays_in_one_block() {
        // A list plus its nested sub-list render as one continuous list block,
        // not split across blocks (which would break the visual nesting).
        let types = block_types("- outer\n  - inner\n- outer two");
        assert_eq!(
            types.iter().filter(|t| *t == "rich_text").count(),
            1,
            "all list elements share one block: {types:?}"
        );
    }

    #[test]
    fn markdown_link_still_wins_over_autolink() {
        // A proper `[label](url)` keeps its label; only bare URLs autolink.
        let els = elements("read [the guide](https://example.com/guide)");
        let link = first_of_type(els[0]["elements"].as_array().unwrap(), "link");
        assert_eq!(link["url"], "https://example.com/guide");
        assert_eq!(link["text"], "the guide");
    }
}

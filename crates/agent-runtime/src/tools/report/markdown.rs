//! Markdown → report spec. Lets `generate_report`/`export_report` typeset a
//! document that already exists instead of making the model hand-transcribe it
//! into IR JSON — the transcription is what produced summarised, hollow reports
//! and tool calls too large to emit.
//!
//! Block level only: [`super::typst_gen`] already styles `**bold**`, `*italic*`
//! and `` `code` `` inside every text field, so inline markup passes through
//! untouched. Links and images are the exception — the IR has neither, so they
//! collapse to their text here rather than reaching the page as literal markup.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

/// Flat `key: value` scalars lifted from a document's front matter.
type FrontMatter = BTreeMap<String, String>;

/// Convert a markdown document to a report spec.
///
/// `default_title` is used only when the document supplies no title of its own —
/// the file stem for a path/artifact source, `"Report"` for inline text — so the
/// result always satisfies the non-empty title `Report::validate` demands.
pub fn to_spec(src: &str, default_title: &str) -> Value {
    let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();
    let (front, body) = split_front_matter(&lines);
    let mut events = parse(body);
    // Front matter is an explicit declaration, so it wins and the body keeps its
    // heading — the convention every static-site generator follows. Only without
    // one is a title inferred from (and consumed out of) the body.
    let declared = front.get("title").filter(|t| !t.trim().is_empty()).cloned();
    let title = declared
        .or_else(|| take_title(&mut events))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| default_title.to_string());

    let mut spec = Map::new();
    spec.insert("title".into(), json!(title));
    for key in ["subtitle", "theme"] {
        if let Some(value) = front.get(key).filter(|v| !v.is_empty()) {
            spec.insert(key.into(), json!(value));
        }
    }
    let meta: Map<String, Value> = ["kicker", "author", "date"]
        .into_iter()
        .filter_map(|key| {
            let value = front.get(key).filter(|v| !v.is_empty())?;
            Some((key.to_string(), json!(value)))
        })
        .collect();
    if !meta.is_empty() {
        spec.insert("meta".into(), Value::Object(meta));
    }
    spec.insert("sections".into(), json!(sections(events)));
    Value::Object(spec)
}

/// One block-level construct, before it is grouped into sections.
#[derive(Debug, PartialEq)]
enum Event {
    Heading { level: u8, text: String },
    Block(Value),
}

/// Split off a leading `---` fenced front-matter chunk, returning its flat
/// `key: value` scalars and the remaining lines. Nested YAML is ignored rather
/// than half-understood; without a closing fence there is no front matter and the
/// leading `---` is left to parse as a thematic break.
fn split_front_matter<'a>(lines: &'a [&'a str]) -> (FrontMatter, &'a [&'a str]) {
    let fenced = matches!(lines.first().map(|l| l.trim()), Some("---"));
    let close = fenced
        .then(|| (1..lines.len()).find(|&k| matches!(lines[k].trim(), "---" | "...")))
        .flatten()
        .filter(|&close| is_front_matter(&lines[1..close]));
    match close {
        Some(close) => (
            lines[1..close]
                .iter()
                .filter_map(|line| line.split_once(':'))
                .map(|(k, v)| {
                    (
                        k.trim().to_ascii_lowercase(),
                        v.trim().trim_matches(['"', '\'']).to_string(),
                    )
                })
                .collect(),
            &lines[close + 1..],
        ),
        None => (FrontMatter::new(), lines),
    }
}

/// Whether a fenced region is front matter rather than the space between two
/// thematic breaks. Every non-blank line has to be front-matter syntax — a
/// `key: value`, a `#` comment, or an indented/`- ` continuation of the key above
/// — and at least one has to be an actual key.
///
/// The scalars this module keeps are a fraction of what a region can hold, so the
/// rest is discarded. That is only safe once the region is known to be front
/// matter: a document opening with a horizontal rule (`---`, prose, `---`) would
/// otherwise have everything above the second rule silently swallowed, which is
/// the exact failure — content that disappears without an error — this whole
/// change exists to remove.
fn is_front_matter(lines: &[&str]) -> bool {
    let mut keyed = false;
    for line in lines {
        let trimmed = line.trim();
        // Blank, a comment, or a continuation of the key above it.
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("- ")
            || line.starts_with([' ', '\t'])
        {
            continue;
        }
        match trimmed.split_once(':') {
            Some((key, _)) if !key.trim().is_empty() => keyed = true,
            _ => return false,
        }
    }
    keyed
}

/// The document's title: the opening heading, consumed so it cannot also become
/// a section.
///
/// Only a heading *shallower* than every heading after it is a title — otherwise
/// it is simply the first section, and taking it would both mis-title the report
/// and silently drop a section heading. A document whose only heading is the
/// first one titles itself with it.
fn take_title(events: &mut Vec<Event>) -> Option<String> {
    let Some(Event::Heading { level, .. }) = events.first() else {
        return None;
    };
    let first = *level;
    let rest = events[1..]
        .iter()
        .filter_map(|e| match e {
            Event::Heading { level, .. } => Some(*level),
            Event::Block(_) => None,
        })
        .min();
    if rest.is_some_and(|shallowest| first >= shallowest) {
        return None;
    }
    match events.remove(0) {
        Event::Heading { text, .. } => Some(text),
        Event::Block(_) => unreachable!("matched as a heading above"),
    }
}

/// Group events into sections, splitting at the shallowest heading level present
/// so a `#`-per-section document and a `#` title + `##`-per-section document both
/// come out right. Deeper headings stay inline as `heading` blocks.
///
/// A section-level heading with no content under it becomes its own heading-only
/// section rather than being folded into the next one: `Report::validate` now
/// rejects a section with no blocks, and prepending the orphan to the following
/// section would render it *after* that section's head, inverting document order.
fn sections(events: Vec<Event>) -> Vec<Value> {
    let split_at = events
        .iter()
        .filter_map(|e| match e {
            Event::Heading { level, .. } => Some(*level),
            Event::Block(_) => None,
        })
        .min();

    let mut out: Vec<Value> = Vec::new();
    let mut heading: Option<String> = None;
    let mut blocks: Vec<Value> = Vec::new();
    for event in events {
        match event {
            Event::Heading { level, text } if Some(level) == split_at => {
                match (blocks.is_empty(), heading.replace(text)) {
                    (false, prev) => out.push(section(prev, std::mem::take(&mut blocks))),
                    (true, Some(orphan)) => out.push(heading_only(orphan)),
                    (true, None) => {}
                }
            }
            Event::Heading { text, .. } => blocks.push(heading_block(text)),
            Event::Block(block) => blocks.push(block),
        }
    }
    match (blocks.is_empty(), heading) {
        (false, heading) => out.push(section(heading, blocks)),
        (true, Some(orphan)) => out.push(heading_only(orphan)),
        (true, None) => {}
    }
    out
}

fn section(heading: Option<String>, blocks: Vec<Value>) -> Value {
    match heading {
        Some(h) => json!({ "heading": h, "blocks": blocks }),
        None => json!({ "blocks": blocks }),
    }
}

fn heading_block(text: String) -> Value {
    json!({ "type": "heading", "text": text })
}

fn heading_only(text: String) -> Value {
    section(None, vec![heading_block(text)])
}

/// Scan the document into block-level events. Line-based with an explicit cursor:
/// every construct here is delimited by line shape, and a fenced block has to be
/// consumed verbatim, which rules out a per-line iterator.
fn parse(lines: &[&str]) -> Vec<Event> {
    let mut events = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if let Some((block, next)) = fenced_code(lines, i) {
            events.push(Event::Block(block));
            i = next;
            continue;
        }
        if let Some((level, text)) = atx_heading(line) {
            events.push(Event::Heading { level, text });
            i += 1;
            continue;
        }
        if let Some((level, text)) = setext_heading(lines, i) {
            events.push(Event::Heading { level, text });
            i += 2;
            continue;
        }
        if is_thematic_break(line) {
            i += 1;
            continue;
        }
        if let Some((table, next)) = gfm_table(lines, i) {
            events.push(Event::Block(table));
            i = next;
            continue;
        }
        if list_marker(line).is_some() {
            let (list, next) = list_block(lines, i);
            events.push(Event::Block(list));
            i = next;
            continue;
        }
        let (text, next) = paragraph(lines, i);
        if !text.is_empty() {
            events.push(Event::Block(json!({ "type": "prose", "text": text })));
        }
        i = next;
    }
    events
}

fn atx_heading(line: &str) -> Option<(u8, String)> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    (1..=6).contains(&hashes).then_some(())?;
    let rest = &trimmed[hashes..];
    (rest.is_empty() || rest.starts_with([' ', '\t'])).then_some(())?;
    let text = rest.trim().trim_end_matches('#').trim();
    (!text.is_empty()).then(|| (hashes as u8, inline_text(text)))
}

/// `Title` over `===` (level 1) or `---` (level 2). Checked before the thematic
/// break so an underlined heading is not mistaken for a horizontal rule.
fn setext_heading(lines: &[&str], i: usize) -> Option<(u8, String)> {
    let text = lines[i].trim();
    let under = lines.get(i + 1)?.trim();
    let marker = under.chars().next()?;
    let level = match marker {
        '=' => 1,
        '-' => 2,
        _ => return None,
    };
    (under.chars().all(|c| c == marker)
        && !text.is_empty()
        && atx_heading(lines[i]).is_none()
        && list_marker(lines[i]).is_none())
    .then(|| (level, inline_text(text)))
}

fn is_thematic_break(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3 && ['-', '*', '_'].iter().any(|m| t.chars().all(|c| c == *m))
}

/// A fence runs to its matching closer, or to end of document when unclosed, and
/// its body is kept line-for-line — indentation is the point of a code block.
fn fenced_code(lines: &[&str], i: usize) -> Option<(Value, usize)> {
    let open = lines[i].trim_start();
    let fence = ["```", "~~~"].into_iter().find(|f| open.starts_with(f))?;
    let language = open[fence.len()..]
        .split_whitespace()
        .next()
        .map(str::to_string);
    let end = (i + 1..lines.len())
        .find(|&k| lines[k].trim_start().starts_with(fence))
        .unwrap_or(lines.len());
    let mut block = Map::new();
    block.insert("type".into(), json!("code"));
    block.insert("text".into(), json!(lines[i + 1..end].join("\n")));
    if let Some(language) = language {
        block.insert("language".into(), json!(language));
    }
    Some((Value::Object(block), (end + 1).min(lines.len())))
}

/// A GFM table needs a header row and a delimiter row; without the delimiter the
/// lines are ordinary text, so the caller falls through to paragraph handling.
fn gfm_table(lines: &[&str], i: usize) -> Option<(Value, usize)> {
    let header = row_cells(lines[i])?;
    is_delimiter_row(lines.get(i + 1)?).then_some(())?;
    let end = (i + 2..lines.len())
        .take_while(|&k| row_cells(lines[k]).is_some())
        .last()
        .map_or(i + 2, |k| k + 1);
    let rows: Vec<Value> = (i + 2..end)
        .filter_map(|k| row_cells(lines[k]))
        .map(|mut cells| {
            cells.resize(header.len(), String::new());
            json!(cells)
        })
        .collect();
    Some((
        json!({ "type": "table", "columns": header, "rows": rows }),
        end,
    ))
}

/// Split a table row on unescaped pipes, dropping the optional leading and
/// trailing delimiters. `None` when the line holds no pipe at all.
fn row_cells(line: &str) -> Option<Vec<String>> {
    let t = line.trim();
    if !t.contains('|') || is_thematic_break(t) {
        return None;
    }
    let mut cells = vec![String::new()];
    let mut escaped = false;
    for c in t.chars() {
        let cell = cells.last_mut().expect("seeded with one cell");
        match (c, escaped) {
            ('\\', false) => escaped = true,
            ('|', false) => cells.push(String::new()),
            ('|', true) => {
                cell.push('|');
                escaped = false;
            }
            (c, was) => {
                if was {
                    cell.push('\\');
                }
                cell.push(c);
                escaped = false;
            }
        }
    }
    if t.starts_with('|') {
        cells.remove(0);
    }
    if t.ends_with('|') && !t.ends_with("\\|") {
        cells.pop();
    }
    (!cells.is_empty()).then(|| cells.iter().map(|c| inline_text(c.trim())).collect())
}

fn is_delimiter_row(line: &str) -> bool {
    row_cells(line).is_some_and(|cells| {
        !cells.is_empty()
            && cells.iter().all(|c| {
                let t = c.trim().trim_start_matches(':').trim_end_matches(':');
                !t.is_empty() && t.chars().all(|c| c == '-')
            })
    })
}

/// Bullet or numbered. Tracked because a change of family ends one list and
/// begins another, so a bullet list followed by a numbered one does not merge.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Marker {
    Bullet,
    Ordered,
}

/// `-`, `*`, `+` or `1.` / `1)`. Returns the indent width and marker family
/// alongside the item text, so the caller can tell nesting from continuation and
/// one list from the next.
fn list_marker(line: &str) -> Option<(usize, Marker, String)> {
    let t = line.trim_start();
    if is_thematic_break(t) {
        return None;
    }
    let (marker, rest) = ["- ", "* ", "+ "]
        .into_iter()
        .find_map(|m| t.strip_prefix(m).map(|rest| (Marker::Bullet, rest)))
        .or_else(|| {
            let digits = t.chars().take_while(char::is_ascii_digit).count();
            (digits > 0)
                .then(|| t.get(digits..))
                .flatten()
                .and_then(|r| r.strip_prefix(". ").or_else(|| r.strip_prefix(") ")))
                .map(|rest| (Marker::Ordered, rest))
        })?;
    Some((line.len() - t.len(), marker, rest.trim().to_string()))
}

/// Consume a run of list items. The IR has no nested list, so a deeper item is
/// flattened into the same list behind an em-dash lead-in: the content survives
/// and the hierarchy stays legible.
fn list_block(lines: &[&str], start: usize) -> (Value, usize) {
    let (base, family) = list_marker(lines[start])
        .map(|(indent, marker, _)| (indent, marker))
        .unwrap_or((0, Marker::Bullet));
    let same_list = |line: &str| {
        list_marker(line).is_some_and(|(indent, marker, _)| indent > base || marker == family)
    };
    let mut items: Vec<Value> = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            // A blank line keeps a loose list together, but only while the marker
            // family holds — otherwise the next list is a separate block.
            if !lines.get(i + 1).is_some_and(|l| same_list(l)) {
                break;
            }
            i += 1;
            continue;
        }
        match list_marker(line) {
            Some((indent, _, text)) if indent > base => items.push(list_item(&format!("— {text}"))),
            Some((_, marker, text)) if marker == family => items.push(list_item(&text)),
            Some(_) => break,
            None if !items.is_empty() && line.starts_with([' ', '\t']) => {
                extend_last(&mut items, line.trim());
            }
            None => break,
        }
        i += 1;
    }
    (json!({ "type": "list", "items": items }), i)
}

/// `**Lead:** rest` becomes a lead/text pair. A bold-only item keeps its markup
/// as text — a lead with nothing after it would render as a dangling label.
fn list_item(text: &str) -> Value {
    let split = text
        .strip_prefix("**")
        .and_then(|r| r.split_once("**"))
        .map(|(lead, rest)| {
            (
                lead.trim().trim_end_matches(':').trim().to_string(),
                rest.trim_start()
                    .trim_start_matches([':', '—', '–', '-'])
                    .trim()
                    .to_string(),
            )
        })
        .filter(|(lead, rest)| !lead.is_empty() && !rest.is_empty());
    match split {
        Some((lead, rest)) => json!({ "lead": inline_text(&lead), "text": inline_text(&rest) }),
        None => json!({ "text": inline_text(text) }),
    }
}

/// Fold a continuation line into the item it belongs to.
fn extend_last(items: &mut [Value], more: &str) {
    if let Some(item) = items.last_mut() {
        let joined = match item["text"].as_str() {
            Some(text) => format!("{text} {}", inline_text(more)),
            None => inline_text(more),
        };
        item["text"] = json!(joined);
    }
}

/// A paragraph runs to a blank line or to the first line that opens another
/// construct, so a list or table straight after prose is not swallowed.
fn paragraph(lines: &[&str], start: usize) -> (String, usize) {
    let end = (start..lines.len())
        .take_while(|&k| !lines[k].trim().is_empty() && (k == start || !opens_construct(lines, k)))
        .last()
        .map_or(start + 1, |k| k + 1);
    let text = lines[start..end]
        .iter()
        .map(|l| strip_quote(l).trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (inline_text(&text), end)
}

fn opens_construct(lines: &[&str], k: usize) -> bool {
    atx_heading(lines[k]).is_some()
        || list_marker(lines[k]).is_some()
        || is_thematic_break(lines[k])
        || fenced_code(lines, k).is_some()
        || gfm_table(lines, k).is_some()
        || setext_heading(lines, k).is_some()
}

fn strip_quote(line: &str) -> &str {
    line.trim_start()
        .strip_prefix('>')
        .map_or(line, |rest| rest.trim_start())
}

/// Collapse the inline constructs the IR cannot carry: an image keeps its alt
/// text, a link keeps its text with the destination in parentheses. Everything
/// else — bold, italic, code — is left for `typst_gen` to style.
fn inline_text(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match link_at(&chars, i) {
            Some((text, dest, next)) => {
                out.push_str(&text);
                if let Some(dest) = dest {
                    out.push_str(&format!(" ({dest})"));
                }
                i = next;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// Match `[text](dest)` at `i`, or `![alt](dest)` which yields no destination —
/// a remote image cannot be embedded, so only its alt text is worth keeping.
fn link_at(chars: &[char], i: usize) -> Option<(String, Option<String>, usize)> {
    let image = chars[i] == '!';
    let open = if image { i + 1 } else { i };
    (*chars.get(open)? == '[').then_some(())?;
    let close = balanced(chars, open, '[', ']')?;
    (*chars.get(close + 1)? == '(').then_some(())?;
    let dest_end = balanced(chars, close + 1, '(', ')')?;
    let text: String = chars[open + 1..close].iter().collect();
    let dest: String = chars[close + 2..dest_end].iter().collect();
    let dest = dest
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    Some((
        text,
        (!image && !dest.is_empty()).then_some(dest),
        dest_end + 1,
    ))
}

/// Index of the `close` that balances the `open` at `from`.
fn balanced(chars: &[char], from: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    (from..chars.len()).find(|&k| {
        match chars[k] {
            c if c == open => depth += 1,
            c if c == close => depth = depth.saturating_sub(1),
            _ => {}
        }
        depth == 0 && chars[k] == close
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::report::ir::Report;

    /// Every test parses through the real IR, so a spec this module emits can
    /// never be shaped in a way the renderer would reject.
    fn spec(src: &str) -> Value {
        let spec = to_spec(src, "Fallback");
        let report: Report =
            serde_json::from_value(spec.clone()).expect("emits a deserialisable spec");
        report.validate().expect("emits a spec that validates");
        spec
    }

    fn headings(spec: &Value) -> Vec<Option<&str>> {
        spec["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["heading"].as_str())
            .collect()
    }

    fn blocks(spec: &Value, section: usize) -> &Vec<Value> {
        spec["sections"][section]["blocks"].as_array().unwrap()
    }

    #[test]
    fn title_comes_from_the_leading_heading_and_sections_from_the_next_level() {
        let s = spec("# Tech Stack\n\n## One\n\nalpha\n\n## Two\n\nbeta\n");
        assert_eq!(s["title"], json!("Tech Stack"));
        assert_eq!(headings(&s), [Some("One"), Some("Two")]);
        assert_eq!(blocks(&s, 0)[0]["text"], json!("alpha"));
    }

    #[test]
    fn a_single_level_document_treats_every_heading_as_a_section() {
        // No heading is shallower than the rest, so none is a title: promoting the
        // first would mis-title the report AND lose a section heading.
        let s = spec("# One\n\nalpha\n\n# Two\n\nbeta\n");
        assert_eq!(s["title"], json!("Fallback"));
        assert_eq!(headings(&s), [Some("One"), Some("Two")]);
    }

    #[test]
    fn a_document_with_one_heading_titles_itself_with_it() {
        let s = spec("## Only Heading\n\nalpha\n");
        assert_eq!(s["title"], json!("Only Heading"));
        assert_eq!(headings(&s), [None]);
        assert_eq!(blocks(&s, 0)[0]["text"], json!("alpha"));
    }

    #[test]
    fn deeper_headings_become_blocks_not_sections() {
        let s = spec("# T\n\n## Section\n\n### Sub\n\nalpha\n\n#### Deeper\n\nbeta\n");
        assert_eq!(headings(&s), [Some("Section")]);
        let b = blocks(&s, 0);
        assert_eq!(b[0], json!({ "type": "heading", "text": "Sub" }));
        assert_eq!(b[2], json!({ "type": "heading", "text": "Deeper" }));
    }

    #[test]
    fn content_before_the_first_section_heading_leads_a_headingless_section() {
        let s = spec("# T\n\nintro prose\n\n## One\n\nalpha\n");
        assert_eq!(headings(&s), [None, Some("One")]);
        assert_eq!(blocks(&s, 0)[0]["text"], json!("intro prose"));
    }

    #[test]
    fn a_heading_with_no_content_keeps_document_order() {
        // `## A` has nothing under it. Folding it into B's section would render it
        // after B's own head, inverting the document — so it gets its own section.
        let s = spec("# T\n\n## A\n\n## B\n\ntext\n");
        assert_eq!(headings(&s), [None, Some("B")]);
        assert_eq!(
            blocks(&s, 0),
            &vec![json!({ "type": "heading", "text": "A" })]
        );
        assert_eq!(blocks(&s, 1)[0]["text"], json!("text"));
    }

    #[test]
    fn a_trailing_heading_with_no_content_is_kept_not_dropped() {
        let s = spec("# T\n\n## A\n\ntext\n\n## Trailing\n");
        assert_eq!(headings(&s), [Some("A"), None]);
        assert_eq!(
            blocks(&s, 1),
            &vec![json!({ "type": "heading", "text": "Trailing" })]
        );
    }

    #[test]
    fn no_heading_at_all_falls_back_to_the_default_title() {
        let s = spec("just prose, no headings\n");
        assert_eq!(s["title"], json!("Fallback"));
        assert_eq!(headings(&s), [None]);
    }

    #[test]
    fn atx_variants_strip_closing_hashes_and_reject_non_headings() {
        assert_eq!(atx_heading("## Heading ##"), Some((2, "Heading".into())));
        assert_eq!(atx_heading("###### Six"), Some((6, "Six".into())));
        assert_eq!(atx_heading("####### Seven"), None, "past level 6");
        assert_eq!(atx_heading("#NoSpace"), None);
        assert_eq!(atx_heading("#"), None, "no text");
    }

    #[test]
    fn setext_headings_parse_and_do_not_collide_with_rules_or_lists() {
        let s = spec("Doc Title\n=========\n\nSection\n-------\n\nalpha\n");
        assert_eq!(s["title"], json!("Doc Title"));
        assert_eq!(headings(&s), [Some("Section")]);
        assert_eq!(
            setext_heading(&["- item", "---"], 0),
            None,
            "list, not text"
        );
    }

    #[test]
    fn thematic_breaks_are_dropped() {
        let s = spec("# T\n\nalpha\n\n---\n\nbeta\n\n***\n\ngamma\n");
        let b = blocks(&s, 0);
        assert_eq!(b.len(), 3, "three paragraphs, no rule blocks: {b:?}");
        assert!(b.iter().all(|x| x["type"] == json!("prose")));
    }

    #[test]
    fn paragraphs_join_wrapped_lines_and_stop_at_the_next_construct() {
        let s = spec("# T\n\nline one\nline two\n- item\n");
        let b = blocks(&s, 0);
        assert_eq!(
            b[0],
            json!({ "type": "prose", "text": "line one line two" })
        );
        assert_eq!(b[1]["type"], json!("list"), "the list is not swallowed");
    }

    #[test]
    fn bullet_ordered_and_continuation_items_all_parse() {
        let s = spec("# T\n\n- first\n* second\n+ third\n\n1. one\n2) two\n");
        let lists: Vec<&Value> = blocks(&s, 0)
            .iter()
            .filter(|b| b["type"] == json!("list"))
            .collect();
        assert_eq!(lists.len(), 2);
        let texts: Vec<&str> = lists[0]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, ["first", "second", "third"]);
        assert_eq!(lists[1]["items"][1]["text"], json!("two"));
    }

    #[test]
    fn a_continuation_line_folds_into_the_item_above_it() {
        let s = spec("# T\n\n- first item\n  wrapped on\n- second\n");
        let items = blocks(&s, 0)[0]["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["text"], json!("first item wrapped on"));
    }

    #[test]
    fn nested_items_flatten_behind_an_em_dash() {
        let s = spec("# T\n\n- top\n  - nested\n- back\n");
        let items = blocks(&s, 0)[0]["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[1]["text"], json!("— nested"));
    }

    #[test]
    fn a_bold_prefix_becomes_a_lead_but_a_bold_only_item_stays_text() {
        assert_eq!(
            list_item("**Orbit:** the platform layer"),
            json!({ "lead": "Orbit", "text": "the platform layer" })
        );
        assert_eq!(
            list_item("**Orbit** — the platform layer"),
            json!({ "lead": "Orbit", "text": "the platform layer" })
        );
        assert_eq!(
            list_item("**Just bold**"),
            json!({ "text": "**Just bold**" }),
            "a lead with no body would render as a dangling label"
        );
    }

    #[test]
    fn gfm_tables_parse_with_alignment_ragged_rows_and_escaped_pipes() {
        let s = spec(
            "# T\n\n| Service | Owner |\n|:---|---:|\n| valuation | ml |\n| intake |\n| a \\| b | c |\n",
        );
        let table = &blocks(&s, 0)[0];
        assert_eq!(table["columns"], json!(["Service", "Owner"]));
        assert_eq!(
            table["rows"],
            json!([["valuation", "ml"], ["intake", ""], ["a | b", "c"]]),
            "short rows pad, escaped pipes survive"
        );
    }

    #[test]
    fn a_pipe_table_without_a_delimiter_row_stays_prose() {
        let s = spec("# T\n\n| a | b |\n| c | d |\n");
        assert_eq!(blocks(&s, 0)[0]["type"], json!("prose"));
    }

    #[test]
    fn fenced_code_keeps_its_lines_and_language() {
        let s = spec("# T\n\n```rust\nfn main() {\n    ok();\n}\n```\n");
        assert_eq!(
            blocks(&s, 0)[0],
            json!({ "type": "code", "text": "fn main() {\n    ok();\n}", "language": "rust" })
        );
    }

    #[test]
    fn fenced_code_without_a_language_and_an_unclosed_fence_both_parse() {
        let s = spec("# T\n\n~~~\nplain\n~~~\n");
        assert_eq!(blocks(&s, 0)[0], json!({ "type": "code", "text": "plain" }));
        let unclosed = spec("# T\n\n```\nstill code\n");
        assert_eq!(blocks(&unclosed, 0)[0]["text"], json!("still code"));
    }

    #[test]
    fn a_fence_is_not_mistaken_for_a_heading_or_a_rule() {
        let s = spec("# T\n\n```\n# not a heading\n---\n```\n");
        let b = blocks(&s, 0);
        assert_eq!(b.len(), 1, "all one code block: {b:?}");
        assert_eq!(b[0]["text"], json!("# not a heading\n---"));
    }

    #[test]
    fn blockquotes_become_prose_without_their_markers() {
        let s = spec("# T\n\n> quoted line\n> and more\n");
        assert_eq!(blocks(&s, 0)[0]["text"], json!("quoted line and more"));
    }

    #[test]
    fn front_matter_supplies_metadata_and_is_never_rendered() {
        let s = spec(
            "---\ntitle: From Front Matter\nsubtitle: A Subtitle\nauthor: \"Ann\"\ndate: July 2026\nkicker: Eng\ntheme: slate\nignored_key: x\n---\n\n## One\n\nalpha\n",
        );
        assert_eq!(s["title"], json!("From Front Matter"));
        assert_eq!(s["subtitle"], json!("A Subtitle"));
        assert_eq!(s["theme"], json!("slate"));
        assert_eq!(
            s["meta"],
            json!({ "author": "Ann", "date": "July 2026", "kicker": "Eng" })
        );
        assert_eq!(headings(&s), [Some("One")]);
        let rendered = serde_json::to_string(&s["sections"]).unwrap();
        assert!(
            !rendered.contains("ignored_key"),
            "front matter not content"
        );
    }

    #[test]
    fn front_matter_title_wins_and_the_body_heading_stays_a_section() {
        let s = spec("---\ntitle: Front\n---\n# Body Heading\n\nalpha\n");
        assert_eq!(s["title"], json!("Front"));
        assert_eq!(
            headings(&s),
            [Some("Body Heading")],
            "an explicitly declared title does not consume a body heading"
        );
    }

    #[test]
    fn an_unclosed_front_matter_fence_is_just_a_thematic_break() {
        let s = spec("---\n# T\n\nalpha\n");
        assert_eq!(s["title"], json!("T"), "no front matter consumed");
    }

    #[test]
    fn a_document_opening_with_a_rule_keeps_the_content_between_the_rules() {
        // Two thematic breaks look exactly like a front-matter fence. Reading the
        // region as front matter discards everything in it that is not a scalar —
        // here the title and a paragraph — with no error: the silent content loss
        // this change exists to remove.
        let s = spec("---\n# Title\n\nalpha\n\n---\n\nbeta\n");
        assert_eq!(s["title"], json!("Title"));
        let texts: Vec<&str> = blocks(&s, 0)
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect();
        assert_eq!(
            texts,
            ["alpha", "beta"],
            "nothing between the rules is lost"
        );
    }

    #[test]
    fn front_matter_is_recognised_through_comments_and_nested_values() {
        // Shapes real front matter has: a `#` comment, a bare key, `- ` items.
        assert!(is_front_matter(&[
            "# generated",
            "title: X",
            "tags:",
            "  - a",
            "- b",
        ]));
        // A key alone proves nothing if there is no key at all.
        assert!(!is_front_matter(&["# just a comment"]));
        assert!(!is_front_matter(&[]));
        // One line of prose is enough to say this is not front matter.
        assert!(!is_front_matter(&["title: X", "ordinary prose here"]));
    }

    #[test]
    fn links_and_images_collapse_to_their_text() {
        assert_eq!(
            inline_text("see [the docs](https://x.test/a) now"),
            "see the docs (https://x.test/a) now"
        );
        assert_eq!(
            inline_text("![a diagram](img.png) follows"),
            "a diagram follows",
            "a remote image cannot be embedded; keep the alt text"
        );
        assert_eq!(
            inline_text("nested [a [b] c](u) end"),
            "nested a [b] c (u) end",
            "brackets balance"
        );
        assert_eq!(
            inline_text("**bold** and `code` untouched"),
            "**bold** and `code` untouched",
            "inline styling is typst_gen's job"
        );
    }

    #[test]
    fn crlf_and_tab_indentation_parse_the_same_as_spaces() {
        let s = spec("# T\r\n\r\n## One\r\n\r\n- item\r\n\t- nested\r\n");
        assert_eq!(headings(&s), [Some("One")]);
        let items = blocks(&s, 0)[0]["items"].as_array().unwrap();
        assert_eq!(items[1]["text"], json!("— nested"));
    }

    #[test]
    fn a_realistic_document_converts_wholly_and_leaves_no_empty_section() {
        let src = "\
# Contoso — Technology Stack Overview

**Technical overview · July 2026**

## 1. Executive summary

Contoso is a vertically integrated platform.

- **Orbit:** the licensed platform layer
- **Valuation:** the AI engine

## 2. Domain architecture

| Service | Owner |
|---|---|
| valuation | ml |
| intake | ops |

### Ownership rules

Each service owns its schema.

```yaml
service: valuation
replicas: 3
```

## 3. What this delivers

> Speed, with guardrails.
";
        let s = spec(src);
        assert_eq!(s["title"], json!("Contoso — Technology Stack Overview"));
        assert_eq!(
            headings(&s),
            [
                None,
                Some("1. Executive summary"),
                Some("2. Domain architecture"),
                Some("3. What this delivers")
            ]
        );
        let kinds: Vec<&str> = blocks(&s, 2)
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["table", "heading", "prose", "code"]);
        assert_eq!(blocks(&s, 1)[1]["items"][0]["lead"], json!("Orbit"));
        assert_eq!(blocks(&s, 3)[0]["text"], json!("Speed, with guardrails."));
    }

    #[test]
    fn an_empty_or_whitespace_document_still_produces_a_titled_spec() {
        // No sections means `validate` rejects, which is correct — but `to_spec`
        // must not panic getting there, and the title must still be filled.
        for src in ["", "   \n\n\t\n"] {
            let s = to_spec(src, "Fallback");
            assert_eq!(s["title"], json!("Fallback"));
            assert_eq!(s["sections"], json!([]));
        }
    }
}

//! Markdown → ratatui `Text` rendering via pulldown-cmark.
//!
//! Converts common markdown elements to styled terminal output:
//! - **bold**, *italic*, ~~strikethrough~~, `inline code`
//! - # Headings (levels 1-3)
//! - Code blocks (with language label)
//! - Bullet and numbered lists
//! - Block quotes
//! - Links (shown as text with URL)

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

/// Render complete markdown to styled ratatui `Text`.
pub fn render_markdown(content: &str) -> Text<'static> {
    if content.trim().is_empty() {
        return Text::raw("");
    }
    convert(content)
}

/// Render partial/streaming markdown — closes unclosed code fences first.
pub fn render_markdown_partial(content: &str) -> Text<'static> {
    if content.trim().is_empty() {
        return Text::raw("");
    }
    let sanitized = sanitize_partial(content);
    convert(&sanitized)
}

/// Close unclosed code fences in partial markdown.
fn sanitize_partial(content: &str) -> String {
    let mut result = content.to_string();
    let fence_count = content.matches("```").count();
    if fence_count % 2 == 1 {
        result.push_str("\n```");
    }
    result
}

// ─── Styles ──────────────────────────────────────────────────────────────────

const CODE_INLINE: Style = Style::new().fg(Color::Yellow).bg(Color::Rgb(40, 40, 50));
const CODE_BLOCK: Style = Style::new()
    .fg(Color::Rgb(180, 210, 230))
    .bg(Color::Rgb(30, 30, 45));
const CODE_LANG: Style = Style::new().fg(Color::DarkGray);
const HEADING1: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const HEADING2: Style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);
const HEADING3: Style = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
const LINK_STYLE: Style = Style::new()
    .fg(Color::Blue)
    .add_modifier(Modifier::UNDERLINED);
const BLOCKQUOTE: Style = Style::new()
    .fg(Color::DarkGray)
    .add_modifier(Modifier::ITALIC);
const LIST_MARKER: Style = Style::new().fg(Color::DarkGray);

// ─── Converter ───────────────────────────────────────────────────────────────

fn convert(content: &str) -> Text<'static> {
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(content, options);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Style> = vec![Style::default()];
    let mut in_code_block = false;
    let mut code_block_lang: String;
    let mut code_block_lines: Vec<String> = Vec::new();
    let mut list_depth: usize = 0;
    let mut list_counters: Vec<Option<u64>> = Vec::new(); // None = unordered, Some(n) = ordered
    let mut in_heading = false;
    let mut heading_style: Style;
    let mut in_blockquote = false;

    // Table state
    let mut in_table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut current_cell = String::new();
    let mut table_alignments: Vec<pulldown_cmark::Alignment> = Vec::new();

    for event in parser {
        match event {
            // ── Inline style tags ────────────────────────────────────────
            Event::Start(Tag::Strong) => {
                let current = current_style(&style_stack);
                style_stack.push(current.add_modifier(Modifier::BOLD));
            }
            Event::End(TagEnd::Strong) => {
                style_stack.pop();
            }
            Event::Start(Tag::Emphasis) => {
                let current = current_style(&style_stack);
                style_stack.push(current.add_modifier(Modifier::ITALIC));
            }
            Event::End(TagEnd::Emphasis) => {
                style_stack.pop();
            }
            Event::Start(Tag::Strikethrough) => {
                let current = current_style(&style_stack);
                style_stack.push(current.add_modifier(Modifier::CROSSED_OUT));
            }
            Event::End(TagEnd::Strikethrough) => {
                style_stack.pop();
            }

            // ── Code ─────────────────────────────────────────────────────
            Event::Code(code) => {
                if in_table {
                    current_cell.push_str(&code);
                } else {
                    current_spans.push(Span::styled(format!(" {} ", code), CODE_INLINE));
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                // Flush current line
                flush_line(&mut lines, &mut current_spans);
                in_code_block = true;
                code_block_lines.clear();
                code_block_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => lang.to_string(),
                    pulldown_cmark::CodeBlockKind::Indented => String::new(),
                };
                if !code_block_lang.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("  ╭─ {} ", code_block_lang),
                        CODE_LANG,
                    )));
                } else {
                    lines.push(Line::from(Span::styled("  ╭───", CODE_LANG)));
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                // Render accumulated code block lines
                for code_line in &code_block_lines {
                    lines.push(Line::from(vec![
                        Span::styled("  │ ", CODE_LANG),
                        Span::styled(code_line.clone(), CODE_BLOCK),
                    ]));
                }
                lines.push(Line::from(Span::styled("  ╰───", CODE_LANG)));
                in_code_block = false;
                code_block_lines.clear();
            }

            // ── Headings ─────────────────────────────────────────────────
            Event::Start(Tag::Heading { level, .. }) => {
                flush_line(&mut lines, &mut current_spans);
                // Add blank line before heading
                if !lines.is_empty() {
                    lines.push(Line::raw(""));
                }
                in_heading = true;
                heading_style = match level {
                    pulldown_cmark::HeadingLevel::H1 => HEADING1,
                    pulldown_cmark::HeadingLevel::H2 => HEADING2,
                    _ => HEADING3,
                };
                style_stack.push(heading_style);
            }
            Event::End(TagEnd::Heading(_)) => {
                style_stack.pop();
                in_heading = false;
                flush_line(&mut lines, &mut current_spans);
            }

            // ── Paragraphs ───────────────────────────────────────────────
            Event::Start(Tag::Paragraph) => {
                if in_blockquote {
                    // Don't add spacing inside blockquotes
                } else if !lines.is_empty() && !in_heading {
                    // Add spacing between paragraphs
                    flush_line(&mut lines, &mut current_spans);
                }
            }
            Event::End(TagEnd::Paragraph) => {
                flush_line(&mut lines, &mut current_spans);
            }

            // ── Lists ────────────────────────────────────────────────────
            Event::Start(Tag::List(start)) => {
                flush_line(&mut lines, &mut current_spans);
                list_depth += 1;
                list_counters.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
                list_counters.pop();
            }
            Event::Start(Tag::Item) => {
                flush_line(&mut lines, &mut current_spans);
                let indent = "  ".repeat(list_depth);
                let marker = if let Some(Some(n)) = list_counters.last_mut() {
                    let m = format!("{}. ", n);
                    *n += 1;
                    m
                } else {
                    "• ".to_string()
                };
                current_spans.push(Span::styled(format!("{}{}", indent, marker), LIST_MARKER));
            }
            Event::End(TagEnd::Item) => {
                flush_line(&mut lines, &mut current_spans);
            }

            // ── Block quotes ─────────────────────────────────────────────
            Event::Start(Tag::BlockQuote(_)) => {
                flush_line(&mut lines, &mut current_spans);
                in_blockquote = true;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush_line(&mut lines, &mut current_spans);
                in_blockquote = false;
            }

            // ── Links ────────────────────────────────────────────────────
            Event::Start(Tag::Link { .. }) => {
                style_stack.push(LINK_STYLE);
            }
            Event::End(TagEnd::Link) => {
                style_stack.pop();
            }

            // ── Tables ────────────────────────────────────────────────────
            Event::Start(Tag::Table(alignments)) => {
                flush_line(&mut lines, &mut current_spans);
                in_table = true;
                table_rows.clear();
                table_alignments = alignments;
            }
            Event::End(TagEnd::Table) => {
                // Render the accumulated table
                render_table(&mut lines, &table_rows, &table_alignments);
                in_table = false;
                table_rows.clear();
                table_alignments.clear();
            }
            Event::Start(Tag::TableHead) => {
                current_row.clear();
            }
            Event::End(TagEnd::TableHead) => {
                table_rows.push(current_row.clone());
                current_row.clear();
            }
            Event::Start(Tag::TableRow) => {
                current_row.clear();
            }
            Event::End(TagEnd::TableRow) => {
                table_rows.push(current_row.clone());
                current_row.clear();
            }
            Event::Start(Tag::TableCell) => {
                current_cell.clear();
            }
            Event::End(TagEnd::TableCell) => {
                current_row.push(current_cell.clone());
                current_cell.clear();
            }

            // ── Text content ─────────────────────────────────────────────
            Event::Text(text) => {
                if in_table {
                    current_cell.push_str(&text);
                } else if in_code_block {
                    // Accumulate code lines (expand tabs to 4 spaces for consistent width)
                    for line in text.split('\n') {
                        code_block_lines.push(line.replace('\t', "    "));
                    }
                    // Remove trailing empty line from split
                    if text.ends_with('\n') {
                        code_block_lines.pop();
                    }
                } else if in_blockquote {
                    for line in text.split('\n') {
                        if !line.is_empty() {
                            current_spans.push(Span::styled("  │ ".to_string(), LIST_MARKER));
                            current_spans.push(Span::styled(line.to_string(), BLOCKQUOTE));
                        }
                        if text.contains('\n') {
                            flush_line(&mut lines, &mut current_spans);
                        }
                    }
                } else {
                    let style = current_style(&style_stack);
                    current_spans.push(Span::styled(text.to_string(), style));
                }
            }

            Event::SoftBreak => {
                if !in_code_block {
                    current_spans.push(Span::raw(" "));
                }
            }
            Event::HardBreak => {
                flush_line(&mut lines, &mut current_spans);
            }

            // ── Horizontal rule ──────────────────────────────────────────
            Event::Rule => {
                flush_line(&mut lines, &mut current_spans);
                lines.push(Line::from(Span::styled(
                    "────────────────────────────────────",
                    Style::new().fg(Color::DarkGray),
                )));
            }

            // Ignore everything else (HTML, footnotes, etc.)
            _ => {}
        }
    }

    // Flush remaining spans
    flush_line(&mut lines, &mut current_spans);

    if lines.is_empty() {
        Text::raw(content.to_string())
    } else {
        Text::from(lines)
    }
}

/// Render a table as aligned columns with box-drawing borders.
fn render_table(
    lines: &mut Vec<Line<'static>>,
    rows: &[Vec<String>],
    _alignments: &[pulldown_cmark::Alignment],
) {
    use unicode_width::UnicodeWidthStr;

    if rows.is_empty() {
        return;
    }

    // Calculate column widths
    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut col_widths = vec![0usize; num_cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < num_cols {
                col_widths[i] = col_widths[i].max(cell.width());
            }
        }
    }

    let table_border = Style::new().fg(Color::DarkGray);
    let header_style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let cell_style = Style::new();

    // Top border
    let top = format!(
        "  ┌{}┐",
        col_widths
            .iter()
            .map(|w| "─".repeat(w + 2))
            .collect::<Vec<_>>()
            .join("┬")
    );
    lines.push(Line::from(Span::styled(top, table_border)));

    for (row_idx, row) in rows.iter().enumerate() {
        // Row content
        let mut spans = vec![Span::styled("  │", table_border)];
        for (i, width) in col_widths.iter().enumerate() {
            let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
            let padding = width.saturating_sub(cell.width());
            let style = if row_idx == 0 {
                header_style
            } else {
                cell_style
            };
            spans.push(Span::styled(
                format!(" {}{} ", cell, " ".repeat(padding)),
                style,
            ));
            spans.push(Span::styled("│", table_border));
        }
        lines.push(Line::from(spans));

        // Separator after header
        if row_idx == 0 {
            let sep = format!(
                "  ├{}┤",
                col_widths
                    .iter()
                    .map(|w| "─".repeat(w + 2))
                    .collect::<Vec<_>>()
                    .join("┼")
            );
            lines.push(Line::from(Span::styled(sep, table_border)));
        }
    }

    // Bottom border
    let bottom = format!(
        "  └{}┘",
        col_widths
            .iter()
            .map(|w| "─".repeat(w + 2))
            .collect::<Vec<_>>()
            .join("┴")
    );
    lines.push(Line::from(Span::styled(bottom, table_border)));
}

fn current_style(stack: &[Style]) -> Style {
    stack.last().copied().unwrap_or_default()
}

fn flush_line(lines: &mut Vec<Line<'static>>, spans: &mut Vec<Span<'static>>) {
    if !spans.is_empty() {
        lines.push(Line::from(std::mem::take(spans)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_closes_unclosed_fence() {
        let input = "Here's code:\n```python\ndef foo():";
        let result = sanitize_partial(input);
        assert!(result.ends_with("\n```"));

        let input2 = "```python\ncode\n```";
        let result2 = sanitize_partial(input2);
        assert_eq!(result2, input2);
    }

    #[test]
    fn render_empty_content() {
        let text = render_markdown("");
        assert!(text.lines.is_empty() || text.lines.len() == 1);
    }

    #[test]
    fn render_basic_markdown() {
        let text = render_markdown("**bold** and *italic*");
        assert!(!text.lines.is_empty());
    }

    #[test]
    fn render_code_block() {
        let text = render_markdown("```rust\nfn main() {}\n```");
        // Should have: lang header, code line, closing border
        assert!(text.lines.len() >= 3);
    }

    #[test]
    fn render_heading() {
        let text = render_markdown("# Hello World");
        assert!(!text.lines.is_empty());
    }

    #[test]
    fn render_list() {
        let text = render_markdown("- one\n- two\n- three");
        assert!(text.lines.len() >= 3);
    }

    #[test]
    fn render_table() {
        let text = render_markdown("| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |");
        // Should have: top border, header, separator, 2 data rows, bottom border
        assert!(
            text.lines.len() >= 6,
            "table has {} lines",
            text.lines.len()
        );
    }

    #[test]
    fn render_nested_list() {
        let text = render_markdown(
            "1. First item\n   - Sub bullet a\n   - Sub bullet b\n2. Second item\n   - Sub bullet c",
        );
        // Should have at least 5 lines for the 5 items
        assert!(
            text.lines.len() >= 5,
            "nested list has {} lines: {:?}",
            text.lines.len(),
            text.lines
                .iter()
                .map(|l| l
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join(""))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn render_table_with_bold() {
        let text = render_markdown(
            "| Feature | Status |\n| --- | --- |\n| **Auth** | Done |\n| `API` | WIP |",
        );
        // Should still render as a table (bold/code in cells captured as text)
        assert!(
            text.lines.len() >= 6,
            "table with bold has {} lines",
            text.lines.len()
        );
    }
}

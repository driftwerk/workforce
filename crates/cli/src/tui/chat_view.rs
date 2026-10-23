//! Scrollable chat history renderer.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{App, ChatMessage, MessageRole};
use super::markdown;
use super::theme;

/// Render the full chat history in the given area.
pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::default().borders(Borders::NONE);

    let inner = block.inner(area);

    // Build all lines from messages, pre-wrapping to terminal width
    let mut all_lines: Vec<Line<'static>> = Vec::new();
    let msg_count = app.messages.len();
    let is_streaming = app.streaming;
    let wrap_width = inner.width as usize;

    for (i, msg) in app.messages.iter_mut().enumerate() {
        // Spacing between messages
        if i > 0 {
            all_lines.push(Line::raw(""));
        }

        // Role label
        let (label, label_style) = match msg.role {
            MessageRole::User => ("you", theme::USER_LABEL),
            MessageRole::Assistant => (
                app.agent_id.as_deref().unwrap_or("agent"),
                theme::ASSISTANT_LABEL,
            ),
            MessageRole::System => ("system", theme::SYSTEM_LABEL),
        };
        all_lines.push(Line::from(Span::styled(
            format!("  {}", label),
            label_style,
        )));

        let border_style = match msg.role {
            MessageRole::User => theme::USER_BORDER,
            MessageRole::Assistant => theme::ASSISTANT_BORDER,
            MessageRole::System => theme::SYSTEM_LABEL,
        };

        // Tool indicators (rendered before content — tools run first, then text streams in)
        for tool in &msg.tools {
            let (icon, style) = if tool.done {
                ("✓", theme::TOOL_DONE)
            } else {
                ("⚙", theme::TOOL_RUNNING)
            };
            let detail = if tool.done && !tool.detail.is_empty() {
                format!(" ({})", tool.detail)
            } else if !tool.done {
                "...".to_string()
            } else {
                String::new()
            };
            all_lines.push(Line::from(vec![
                Span::styled("  ┃ ", border_style),
                Span::styled(format!("  {} {}{}", icon, tool.name, detail), style),
            ]));
        }

        // Content — pre-wrap each line so wrapped continuations get the border
        let is_last_and_streaming = is_streaming && i == msg_count - 1;
        let content_lines = render_message_content(msg, is_last_and_streaming);

        for line in content_lines {
            let wrapped = wrap_content_line(line, border_style, wrap_width);
            all_lines.extend(wrapped);
        }
    }

    // Typing / waiting indicator
    if app.waiting_for_response
        && !app.streaming
        && app
            .messages
            .last()
            .is_none_or(|m| m.role != MessageRole::Assistant)
    {
        all_lines.push(Line::raw(""));
        all_lines.push(Line::from(Span::styled(
            format!("  {}", app.agent_id.as_deref().unwrap_or("agent")),
            theme::ASSISTANT_LABEL,
        )));
        let dots = match (app.tick_count / 2) % 4 {
            0 => "·",
            1 => "··",
            2 => "···",
            _ => "",
        };
        all_lines.push(Line::from(vec![
            Span::styled("  ┃ ", theme::ASSISTANT_BORDER),
            Span::styled(
                dots.to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]));
    }

    // Empty state
    if all_lines.is_empty() {
        all_lines.push(Line::raw(""));
        all_lines.push(Line::from(Span::styled(
            "  Start a conversation...",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    // Lines are pre-wrapped, so each Line is one visual row.
    let total_visual = all_lines.len() as u16;
    app.total_lines = total_visual;

    // Compute scroll position (offset from bottom)
    let visible_height = inner.height;
    let max_scroll = total_visual.saturating_sub(visible_height);
    let scroll_offset = app.scroll_offset.min(max_scroll);
    let scroll_pos = max_scroll.saturating_sub(scroll_offset);

    let paragraph = Paragraph::new(Text::from(all_lines))
        .block(block)
        .scroll((scroll_pos, 0));

    frame.render_widget(paragraph, area);

    // Scroll indicator
    if scroll_offset > 0 {
        let indicator = Span::styled(
            format!(" ↑ {} lines above ", scroll_offset),
            theme::SCROLL_INDICATOR,
        );
        let indicator_line = Line::from(indicator);
        let indicator_area = Rect {
            x: area.x + area.width.saturating_sub(25),
            y: area.y,
            width: 25.min(area.width),
            height: 1,
        };
        frame.render_widget(Paragraph::new(indicator_line), indicator_area);
    }
}

/// Render a single message's content to lines.
fn render_message_content(msg: &mut ChatMessage, is_streaming: bool) -> Vec<Line<'static>> {
    if msg.content.is_empty() {
        return vec![];
    }

    match msg.role {
        MessageRole::Assistant => {
            // Use cached render if available
            if let Some(ref rendered) = msg.rendered {
                return rendered.lines.clone();
            }

            // Render markdown
            let text = if is_streaming {
                markdown::render_markdown_partial(&msg.content)
            } else {
                markdown::render_markdown(&msg.content)
            };

            let lines = text.lines.clone();

            // Cache if not streaming (streaming invalidates every delta)
            if !is_streaming {
                msg.rendered = Some(text);
            }

            lines
        }
        MessageRole::System => {
            // Render markdown for system messages (tables, etc.)
            if let Some(ref rendered) = msg.rendered {
                return rendered.lines.clone();
            }
            let text = markdown::render_markdown(&msg.content);
            let lines = text.lines.clone();
            msg.rendered = Some(text);
            lines
        }
        MessageRole::User => {
            // Plain text rendering
            msg.content
                .lines()
                .map(|line| Line::raw(line.to_string()))
                .collect()
        }
    }
}

// ─── Line wrapping ──────────────────────────────────────────────────────────

/// Width of the border prefix `"  ┃ "`.
const BORDER_WIDTH: usize = 4;

/// Pre-wrap a content line to fit within `max_width`, prepending the `┃` border
/// to every resulting visual line (including continuations).
///
/// If the line starts with an inner decoration prefix (e.g. `"  │ "` for code
/// blocks or blockquotes), that prefix is repeated on every continuation line so
/// the nested visual boundary stays intact.
fn wrap_content_line(
    line: Line<'static>,
    border_style: Style,
    max_width: usize,
) -> Vec<Line<'static>> {
    let content_width = max_width.saturating_sub(BORDER_WIDTH);
    if content_width == 0 {
        return vec![Line::from(Span::styled("  ┃ ", border_style))];
    }

    // Detect an inner border prefix that should repeat on continuations.
    let (inner_prefix, body_spans) = extract_inner_prefix(line.spans);
    let prefix_width = inner_prefix
        .as_ref()
        .map(|s| s.content.width())
        .unwrap_or(0);
    let body_width = content_width.saturating_sub(prefix_width);

    if body_width == 0 {
        return vec![Line::from(Span::styled("  ┃ ", border_style))];
    }

    let visual_lines = wrap_spans(body_spans, body_width);

    visual_lines
        .into_iter()
        .map(|spans| {
            let mut result = vec![Span::styled("  ┃ ", border_style)];
            if let Some(ref pfx) = inner_prefix {
                result.push(pfx.clone());
            }
            result.extend(spans);
            Line::from(result)
        })
        .collect()
}

/// If the first span is a known inner decoration (code block `"  │ "` or
/// blockquote `"  │ "`), extract it so it can be repeated on wrapped continuations.
fn extract_inner_prefix(
    mut spans: Vec<Span<'static>>,
) -> (Option<Span<'static>>, Vec<Span<'static>>) {
    if let Some(first) = spans.first() {
        let text = first.content.as_ref();
        // Match the code-block / blockquote prefix produced by markdown.rs
        if text == "  │ " {
            let prefix = spans.remove(0);
            return (Some(prefix), spans);
        }
    }
    (None, spans)
}

/// Break a sequence of styled spans into visual lines that fit within `max_width`.
/// Prefers breaking at word boundaries (spaces).
fn wrap_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Vec<Span<'static>>> {
    if max_width == 0 {
        return vec![vec![]];
    }

    let mut visual_lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current_line: Vec<Span<'static>> = Vec::new();
    let mut line_width: usize = 0;

    for span in spans {
        let style = span.style;
        let full_text: String = span.content.into_owned();
        let mut remaining = full_text.as_str();

        while !remaining.is_empty() {
            let remaining_width = remaining.width();
            let available = max_width.saturating_sub(line_width);

            if remaining_width <= available {
                // Entire remaining text fits on this line
                current_line.push(Span::styled(remaining.to_string(), style));
                line_width += remaining_width;
                break;
            }

            if available == 0 {
                // Line is full — start a new one
                visual_lines.push(std::mem::take(&mut current_line));
                line_width = 0;
                continue;
            }

            // Find where to break
            let (take_bytes, skip_bytes) = find_break_point(remaining, available);

            if take_bytes > 0 {
                current_line.push(Span::styled(remaining[..take_bytes].to_string(), style));
            }

            // Flush line and continue with the rest
            visual_lines.push(std::mem::take(&mut current_line));
            line_width = 0;
            remaining = &remaining[skip_bytes..];
        }
    }

    // Flush the last line
    if !current_line.is_empty() || visual_lines.is_empty() {
        visual_lines.push(current_line);
    }

    visual_lines
}

/// Find where to break `text` so the taken portion fits within `max_width` columns.
/// Returns `(bytes_to_take, bytes_to_skip_past)`. Prefers breaking after the last
/// space that fits; falls back to a hard character break.
fn find_break_point(text: &str, max_width: usize) -> (usize, usize) {
    let mut width = 0;
    let mut last_space_take = None;
    let mut last_space_skip = None;

    for (byte_idx, ch) in text.char_indices() {
        let ch_width = ch.width().unwrap_or(0);

        if width + ch_width > max_width {
            // Over the limit — break at last space or hard-break here
            if let (Some(take), Some(skip)) = (last_space_take, last_space_skip) {
                return (take, skip);
            }
            // If the very first char doesn't fit, take it anyway to avoid an
            // infinite loop in wrap_spans (always make forward progress).
            if byte_idx == 0 {
                let next = byte_idx + ch.len_utf8();
                return (next, next);
            }
            return (byte_idx, byte_idx);
        }

        width += ch_width;

        if ch == ' ' {
            last_space_take = Some(byte_idx);
            last_space_skip = Some(byte_idx + 1); // skip the space on the next line
        }
    }

    // Entire text fits (shouldn't normally reach here given caller logic)
    (text.len(), text.len())
}

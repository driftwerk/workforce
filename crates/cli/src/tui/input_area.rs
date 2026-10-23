//! Input area widget — wraps tui-textarea with custom Enter/Ctrl+Enter handling.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use super::app::{format_size, App, AttachmentKind, InputMode};
use super::theme;

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let has_attachments = !app.pending_attachments.is_empty();

    if has_attachments {
        // Split area: 1 row for attachments, rest for textarea
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(area);

        render_attachment_bar(frame, chunks[0], app);
        render_textarea(frame, chunks[1], app);
    } else {
        render_textarea(frame, area, app);
    }
}

fn render_attachment_bar(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = Vec::new();
    for (i, att) in app.pending_attachments.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let icon = match att.kind {
            AttachmentKind::Image => "img",
            AttachmentKind::Text => "txt",
            AttachmentKind::Binary => "doc",
        };
        spans.push(Span::styled(
            format!(" [{}] {} ({}) ", icon, att.filename, format_size(att.size)),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ));
    }
    spans.push(Span::styled(
        "  Ctrl+X to remove",
        Style::default().fg(Color::DarkGray),
    ));
    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_textarea(frame: &mut Frame, area: Rect, app: &mut App) {
    let border_style = match app.input_mode {
        InputMode::Normal => theme::INPUT_BORDER_ACTIVE,
        InputMode::Scrolling => theme::INPUT_BORDER,
    };

    let title = if app.input_mode == InputMode::Scrolling {
        " Input (press i or Enter to type) "
    } else {
        " Input (Ctrl+Enter = newline) "
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);

    app.input.set_block(block);
    frame.render_widget(&app.input, area);
}

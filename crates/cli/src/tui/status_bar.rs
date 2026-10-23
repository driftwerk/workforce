//! Bottom status bar — session, message count, model, context, connection.

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::app::{App, ConnectionStatus, InputMode};
use super::theme;

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let session = app
        .session_id
        .as_deref()
        .map(|s| &s[..s.len().min(8)])
        .unwrap_or("...");

    let connection = match app.connection_status {
        ConnectionStatus::Connected => Span::styled("● Online", theme::STATUS_CONNECTED),
        ConnectionStatus::Disconnected => Span::styled("● Offline", theme::STATUS_DISCONNECTED),
        ConnectionStatus::Reconnecting(n) => Span::styled(
            format!("● Reconnecting ({})", n),
            theme::STATUS_RECONNECTING,
        ),
    };

    let mode = match app.input_mode {
        InputMode::Normal => Span::raw(""),
        InputMode::Scrolling => Span::styled(" [SCROLL] ", theme::SCROLL_INDICATOR),
    };

    let mouse_mode = if !app.mouse_capture {
        Span::styled(" [COPY] ", theme::SCROLL_INDICATOR)
    } else {
        Span::raw("")
    };

    let streaming = if app.compacting {
        let dots = match (app.tick_count / 2) % 4 {
            0 => ".",
            1 => "..",
            2 => "...",
            _ => "",
        };
        Span::styled(format!(" compacting{} ", dots), theme::STATUS_RECONNECTING)
    } else if app.streaming {
        let dots = match (app.tick_count / 2) % 4 {
            0 => ".",
            1 => "..",
            2 => "...",
            _ => "",
        };
        Span::styled(format!(" streaming{} ", dots), theme::STATUS_RECONNECTING)
    } else if app.waiting_for_response {
        let dots = match (app.tick_count / 2) % 4 {
            0 => ".",
            1 => "..",
            2 => "...",
            _ => "",
        };
        Span::styled(format!(" thinking{} ", dots), theme::STATUS_RECONNECTING)
    } else {
        Span::raw("")
    };

    let mut parts = vec![
        Span::raw(" "),
        Span::styled(format!("session:{}", session), theme::STATUS_SESSION),
        Span::raw(" │ "),
        Span::raw(format!("{} msgs", app.message_count)),
    ];

    // Show agent name when explicitly set
    if let Some(ref agent) = app.agent_id {
        parts.push(Span::raw(" │ "));
        parts.push(Span::styled(format!("@{}", agent), theme::STATUS_SESSION));
    }

    parts.push(Span::raw(" │ "));
    parts.push(Span::styled(app.model.clone(), theme::STATUS_SESSION));

    // Context window usage bar
    if app.context_max > 0 {
        let pct = (app.context_tokens as f64 / app.context_max as f64 * 100.0).min(100.0);
        let bar_width: usize = 8;
        let filled = (pct / 100.0 * bar_width as f64).round() as usize;
        let empty = bar_width.saturating_sub(filled);

        let bar_color = if pct >= 80.0 {
            Color::Red
        } else if pct >= 60.0 {
            Color::Yellow
        } else {
            Color::Green
        };

        parts.push(Span::raw(" │ "));
        parts.push(Span::styled(
            format!(
                "ctx [{}{}] {:.0}%",
                "█".repeat(filled),
                "░".repeat(empty),
                pct
            ),
            Style::default().fg(bar_color),
        ));
    }

    parts.push(Span::raw(" │ "));
    parts.push(connection);
    parts.push(mode);
    parts.push(mouse_mode);
    parts.push(streaming);

    let line = Line::from(parts);

    let bar = Paragraph::new(line).style(theme::STATUS_BAR);
    frame.render_widget(bar, area);
}

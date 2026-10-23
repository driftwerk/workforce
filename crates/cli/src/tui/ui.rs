//! Top-level UI layout — composes chat view, input area, and status bar.

use ratatui::{
    layout::{Constraint, Direction, Layout},
    Frame,
};

use super::app::App;
use super::{chat_view, input_area, status_bar};

pub fn draw(frame: &mut Frame, app: &mut App) {
    // Input height: 3 lines minimum (border + 1 line + border), up to 8 for multi-line.
    // Add 1 extra row when attachments are pending (for the attachment indicator bar).
    let input_lines = app.input.lines().len() as u16;
    let attachment_row = if app.pending_attachments.is_empty() {
        0
    } else {
        1
    };
    let input_height =
        (input_lines + 2 + attachment_row).clamp(3 + attachment_row, 8 + attachment_row);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(4),               // chat history
            Constraint::Length(1),            // spacer
            Constraint::Length(input_height), // input area
            Constraint::Length(1),            // status bar
        ])
        .split(frame.area());

    chat_view::render(frame, chunks[0], app);
    // chunks[1] is an empty spacer row
    input_area::render(frame, chunks[2], app);
    status_bar::render(frame, chunks[3], app);
}

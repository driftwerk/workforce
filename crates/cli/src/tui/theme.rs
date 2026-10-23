//! Color and style constants for the TUI.

use ratatui::style::{Color, Modifier, Style};

// ─── Role colors ─────────────────────────────────────────────────────────────

pub const USER_LABEL: Style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
pub const USER_BORDER: Style = Style::new().fg(Color::Green);

pub const ASSISTANT_LABEL: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
pub const ASSISTANT_BORDER: Style = Style::new().fg(Color::DarkGray);

pub const SYSTEM_LABEL: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::DIM);

// ─── Tool indicators ─────────────────────────────────────────────────────────

pub const TOOL_RUNNING: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::DIM);
pub const TOOL_DONE: Style = Style::new().fg(Color::Green).add_modifier(Modifier::DIM);

// ─── Status bar ──────────────────────────────────────────────────────────────

pub const STATUS_BAR: Style = Style::new().fg(Color::Gray).bg(Color::Rgb(30, 30, 40));
pub const STATUS_SESSION: Style = Style::new().fg(Color::Cyan);
pub const STATUS_CONNECTED: Style = Style::new().fg(Color::Green);
pub const STATUS_DISCONNECTED: Style = Style::new().fg(Color::Red);
pub const STATUS_RECONNECTING: Style = Style::new().fg(Color::Yellow);

// ─── Input area ──────────────────────────────────────────────────────────────

pub const INPUT_BORDER: Style = Style::new().fg(Color::DarkGray);
pub const INPUT_BORDER_ACTIVE: Style = Style::new().fg(Color::Blue);
// ─── Misc ────────────────────────────────────────────────────────────────────

pub const SCROLL_INDICATOR: Style = Style::new().fg(Color::Yellow);

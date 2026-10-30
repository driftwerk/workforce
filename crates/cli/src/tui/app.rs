//! Application state and event-driven update logic.

use std::path::PathBuf;

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::text::Text;
use ratatui_textarea::{CursorMove, TextArea};

use workforce_gateway_client::protocol::{ServerMessageKind, TranscriptEntryDto};
use workforce_gateway_client::GatewayEvent;

use super::event::Event;

// ─── Actions ─────────────────────────────────────────────────────────────────

/// Actions that the main loop must handle (some require async work).
pub enum Action {
    /// Render a frame.
    Render,
    /// Send a chat message to the gateway.
    SendMessage(String),
    /// Execute a local command (starts with /).
    LocalCommand(String),
    /// Cancel the current stream/response.
    CancelStream,
    /// Toggle mouse capture on/off (for copy/paste).
    ToggleMouseCapture,
    /// Attach a file (from drag-drop or /attach command).
    AttachFile(PathBuf),
    /// Remove the last pending attachment.
    RemoveAttachment,
    /// Quit the application.
    Quit,
    /// No-op.
    None,
}

// ─── Tab completion ─────────────────────────────────────────────────────────

/// All slash commands available for tab-completion (sorted alphabetically).
const SLASH_COMMANDS: &[&str] = &[
    "/agent",
    "/agents",
    "/attach",
    "/compact",
    "/context",
    "/ctx",
    "/exit",
    "/gc",
    "/h",
    "/hb",
    "/health",
    "/heartbeat",
    "/help",
    "/m",
    "/mode",
    "/model",
    "/n",
    "/new",
    "/q",
    "/quit",
    "/r",
    "/reset",
    "/s",
    "/sa",
    "/select-agent",
    "/sessions",
    "/status",
    "/switch",
    "/t",
    "/tools",
    "/w",
    "/wake",
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum CompletionKind {
    SlashCommand,
    FilePath,
    SessionId,
    AgentId,
}

#[derive(Debug, Clone)]
struct CompletionState {
    kind: CompletionKind,
    candidates: Vec<String>,
    index: usize,
}

// ─── Attachments ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Text,
    Image,
    Binary,
}

#[derive(Debug, Clone)]
pub struct PendingAttachment {
    pub path: PathBuf,
    pub filename: String,
    pub kind: AttachmentKind,
    pub size: u64,
}

pub fn classify_file(path: &std::path::Path) -> AttachmentKind {
    if let Some(ext) = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
    {
        match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" => return AttachmentKind::Image,
            "xlsx" | "xls" | "ods" | "pdf" | "docx" | "doc" | "odt" | "pptx" | "ppt" => {
                return AttachmentKind::Binary;
            }
            _ => {}
        }
    }
    // Fall back to magic bytes (handles hidden extensions on macOS, etc.)
    if detect_image_magic(path).is_some() {
        AttachmentKind::Image
    } else {
        AttachmentKind::Text
    }
}

/// Detect image type from file magic bytes. Returns the MIME type if recognized.
pub fn detect_image_magic(path: &std::path::Path) -> Option<&'static str> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 12];
    file.read_exact(&mut buf).ok()?;

    if buf.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if buf.starts_with(b"\xFF\xD8\xFF") {
        Some("image/jpeg")
    } else if buf.starts_with(b"GIF87a") || buf.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if buf.starts_with(b"RIFF") && &buf[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Get MIME type for a file — checks extension first, then magic bytes.
pub fn mime_for_file(path: &std::path::Path) -> &'static str {
    // Try extension
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        match ext.to_lowercase().as_str() {
            "png" => return "image/png",
            "jpg" | "jpeg" => return "image/jpeg",
            "gif" => return "image/gif",
            "webp" => return "image/webp",
            "xlsx" => return "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "xls" => return "application/vnd.ms-excel",
            "ods" => return "application/vnd.oasis.opendocument.spreadsheet",
            "pdf" => return "application/pdf",
            "docx" => {
                return "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            }
            "doc" => return "application/msword",
            "odt" => return "application/vnd.oasis.opendocument.text",
            "pptx" => {
                return "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            }
            "ppt" => return "application/vnd.ms-powerpoint",
            _ => {}
        }
    }
    // Fall back to magic bytes
    detect_image_magic(path).unwrap_or("text/plain")
}

pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Try to parse a pasted string as a dropped file path.
///
/// Handles macOS quirks: backslash-escaped spaces (`my\ file.png`),
/// quoted paths, `file://` URLs, and trailing whitespace.
fn parse_dropped_path(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }

    // Try several cleaning strategies
    let candidates: Vec<String> = vec![
        // 1. Raw (after stripping surrounding quotes)
        trimmed.trim_matches(|c| c == '\'' || c == '"').to_string(),
        // 2. Unescape backslash-space and backslash-paren (macOS Terminal.app)
        trimmed
            .trim_matches(|c| c == '\'' || c == '"')
            .replace("\\ ", " ")
            .replace("\\(", "(")
            .replace("\\)", ")")
            .replace("\\[", "[")
            .replace("\\]", "]"),
        // 3. file:// URL (Finder sometimes uses this)
        {
            let s = trimmed.trim_matches(|c| c == '\'' || c == '"');
            if let Some(stripped) = s.strip_prefix("file://") {
                // URL-decode percent-encoded characters
                url_decode(stripped)
            } else {
                String::new()
            }
        },
    ];

    for candidate in candidates {
        if candidate.is_empty() {
            continue;
        }
        let path = PathBuf::from(&candidate);
        if path.exists() && path.is_file() {
            return Some(path);
        }
    }

    None
}

/// Simple percent-decoding for file:// URLs (e.g. `%20` → ` `).
fn url_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            } else {
                result.push('%');
                result.push_str(&hex);
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Convert a completion candidate back into the full input text.
fn completion_to_input(kind: &CompletionKind, candidate: &str) -> String {
    match kind {
        CompletionKind::SlashCommand => candidate.to_string(),
        CompletionKind::FilePath => format!("/attach {}", candidate),
        CompletionKind::SessionId => format!("/switch {}", candidate),
        CompletionKind::AgentId => format!("/agent {}", candidate),
    }
}

/// List filesystem entries matching a partial path for tab-completion.
///
/// Directories in results get a trailing `/` so the user can Tab again to drill deeper.
fn complete_file_paths(partial: &str) -> Vec<String> {
    use std::path::Path;

    let path = Path::new(partial);

    let (dir, prefix) = if partial.is_empty() {
        (Path::new("."), "")
    } else if partial.ends_with('/') || partial.ends_with(std::path::MAIN_SEPARATOR) {
        (path, "")
    } else {
        let parent = path.parent().unwrap_or(Path::new("."));
        let file_prefix = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
        (parent, file_prefix)
    };

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let dir_str = if partial.is_empty() || partial.ends_with('/') {
        partial.to_string()
    } else {
        match Path::new(partial).parent() {
            Some(p) if p == Path::new("") => String::new(),
            Some(p) => format!("{}/", p.display()),
            None => String::new(),
        }
    };

    let mut candidates: Vec<String> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_str()?.to_string();

            // Skip hidden files unless the prefix starts with '.'
            if name.starts_with('.') && !prefix.starts_with('.') {
                return None;
            }
            if !name.starts_with(prefix) {
                return None;
            }

            let is_dir = entry.file_type().ok()?.is_dir();
            let suffix = if is_dir { "/" } else { "" };
            Some(format!("{}{}{}", dir_str, name, suffix))
        })
        .collect();

    candidates.sort();
    candidates
}

// ─── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Typing in the input area.
    Normal,
    /// Navigating chat history with arrow keys.
    Scrolling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connected,
    Disconnected,
    Reconnecting(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone)]
pub struct ToolActivity {
    pub name: String,
    pub done: bool,
    pub detail: String,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    pub tools: Vec<ToolActivity>,
    pub timestamp: chrono::DateTime<Utc>,
    /// Cached rendered text — set to `None` to force re-render.
    pub rendered: Option<Text<'static>>,
}

// ─── App state ───────────────────────────────────────────────────────────────

pub struct App {
    // Chat
    pub messages: Vec<ChatMessage>,
    pub streaming: bool,
    pub stream_buffer: String,
    pub waiting_for_response: bool,
    pub stream_cancelled: bool,
    /// True after user sends a new message — allows next StreamStart to clear cancelled state.
    pub pending_new_request: bool,
    /// Where the current model call began: the message count then, and
    /// whether the last message was an empty assistant one its stream reuses.
    /// `None` until a call begins, so a discard never reaches older messages.
    call_anchor: Option<(usize, bool)>,

    // Input
    pub input: TextArea<'static>,
    pub input_mode: InputMode,
    pub input_history: Vec<String>,
    pub history_index: Option<usize>,

    // Scroll
    pub scroll_offset: u16,
    pub auto_scroll: bool,
    pub total_lines: u16,

    // Connection
    pub connection_status: ConnectionStatus,
    pub session_id: Option<String>,
    pub model: String,
    pub agent_id: Option<String>,
    pub message_count: usize,

    // UI
    pub should_quit: bool,
    pub show_help: bool,
    pub tick_count: u64,
    /// Whether mouse capture is active (false = native text selection works).
    pub mouse_capture: bool,

    // Attachments
    pub pending_attachments: Vec<PendingAttachment>,

    // Compaction
    pub compacting: bool,

    // Context window usage
    pub context_tokens: u32,
    pub context_max: u32,

    // Gateway info
    pub gateway_url: String,

    // Cached data for tab completion
    /// (session_id, label) pairs for /switch completion.
    pub cached_sessions: Vec<(String, Option<String>)>,
    /// (agent_id, name) pairs for /agent completion.
    pub cached_agents: Vec<(String, Option<String>)>,

    // Tab completion
    completion: Option<CompletionState>,
}

impl App {
    pub fn new(model: String, agent_id: Option<String>, gateway_url: String) -> Self {
        let mut input = TextArea::default();
        input.set_cursor_line_style(ratatui::style::Style::default());
        input.set_placeholder_text("Type a message... (Option+Enter for newline)");

        Self {
            messages: Vec::new(),
            streaming: false,
            stream_buffer: String::new(),
            waiting_for_response: false,
            stream_cancelled: false,
            pending_new_request: false,
            call_anchor: None,

            input,
            input_mode: InputMode::Normal,
            input_history: Vec::new(),
            history_index: None,

            scroll_offset: 0,
            auto_scroll: true,
            total_lines: 0,

            connection_status: ConnectionStatus::Disconnected,
            session_id: None,
            model,
            agent_id,
            message_count: 0,

            should_quit: false,
            show_help: false,
            tick_count: 0,
            mouse_capture: true,

            pending_attachments: Vec::new(),

            compacting: false,

            context_tokens: 0,
            context_max: 200_000,

            gateway_url,

            cached_sessions: Vec::new(),
            cached_agents: Vec::new(),

            completion: None,
        }
    }

    /// Process an event and return an action for the main loop to execute.
    pub fn handle_event(&mut self, event: Event) -> Action {
        match event {
            Event::Render => Action::Render,
            Event::Tick => {
                self.tick_count = self.tick_count.wrapping_add(1);
                Action::None
            }
            Event::Terminal(crossterm::event::Event::Key(key)) => self.handle_key(key),
            Event::Terminal(crossterm::event::Event::Mouse(mouse)) => self.handle_mouse(mouse),
            Event::Terminal(crossterm::event::Event::Paste(text)) => {
                // Detect file paths from drag-and-drop
                if let Some(path) = parse_dropped_path(&text) {
                    Action::AttachFile(path)
                } else {
                    self.input.insert_str(&text);
                    Action::None
                }
            }
            Event::Terminal(crossterm::event::Event::Resize(_, _)) => Action::Render,
            Event::Terminal(_) => Action::None,
            Event::Gateway(gw) => self.handle_gateway_event(gw),
        }
    }

    // ─── Mouse handling ───────────────────────────────────────────────────

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Action {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_up(3);
                Action::None
            }
            MouseEventKind::ScrollDown => {
                self.scroll_down(3);
                Action::None
            }
            _ => Action::None,
        }
    }

    // ─── Key handling ────────────────────────────────────────────────────

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        // Global: Ctrl+C always quits
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }

        match self.input_mode {
            InputMode::Normal => self.handle_key_normal(key),
            InputMode::Scrolling => self.handle_key_scrolling(key),
        }
    }

    fn handle_key_normal(&mut self, key: KeyEvent) -> Action {
        match (key.code, key.modifiers) {
            // Tab → complete forward
            (KeyCode::Tab, KeyModifiers::NONE) => {
                self.try_complete(false);
                Action::None
            }
            // Shift+Tab → complete backward
            (KeyCode::BackTab, _) => {
                self.try_complete(true);
                Action::None
            }
            // Enter (no modifier) → send message
            (KeyCode::Enter, m)
                if !m.contains(KeyModifiers::CONTROL)
                    && !m.contains(KeyModifiers::SHIFT)
                    && !m.contains(KeyModifiers::ALT) =>
            {
                self.completion = None;
                // Don't send while the agent is still responding
                if self.waiting_for_response || self.streaming {
                    return Action::None;
                }
                let text: String = self.input.lines().join("\n").trim().to_string();
                if text.is_empty() {
                    return Action::None;
                }

                // Reset input
                self.input = TextArea::default();
                self.input
                    .set_cursor_line_style(ratatui::style::Style::default());
                self.input
                    .set_placeholder_text("Type a message... (Option+Enter for newline)");
                self.history_index = None;

                // Save to history
                self.input_history.push(text.clone());

                if text.starts_with('/') {
                    Action::LocalCommand(text)
                } else {
                    Action::SendMessage(text)
                }
            }
            // Ctrl+Enter, Shift+Enter, or Option+Enter → newline
            (KeyCode::Enter, _) => {
                self.completion = None;
                self.input.insert_newline();
                Action::None
            }
            // Ctrl+U → clear entire input
            (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.completion = None;
                self.set_input_text("");
                Action::None
            }
            // Ctrl+X → remove last pending attachment
            (KeyCode::Char('x'), m)
                if m.contains(KeyModifiers::CONTROL) && !self.pending_attachments.is_empty() =>
            {
                self.completion = None;
                Action::RemoveAttachment
            }
            // Ctrl+Y → toggle mouse capture for copy/paste
            (KeyCode::Char('y'), m) if m.contains(KeyModifiers::CONTROL) => {
                Action::ToggleMouseCapture
            }
            // Escape → cancel stream if active, otherwise scroll mode
            (KeyCode::Esc, _) if self.streaming || self.waiting_for_response => {
                self.completion = None;
                Action::CancelStream
            }
            (KeyCode::Esc, _) => {
                self.completion = None;
                self.input_mode = InputMode::Scrolling;
                Action::None
            }
            // Up arrow: if single line, cycle history
            (KeyCode::Up, KeyModifiers::NONE) if self.input.lines().len() <= 1 => {
                self.completion = None;
                self.cycle_history_back();
                Action::None
            }
            // Down arrow: if single line, cycle history forward
            (KeyCode::Down, KeyModifiers::NONE) if self.input.lines().len() <= 1 => {
                self.completion = None;
                self.cycle_history_forward();
                Action::None
            }
            // Everything else → forward to textarea
            _ => {
                self.completion = None;
                self.input.input(key);
                Action::None
            }
        }
    }

    fn handle_key_scrolling(&mut self, key: KeyEvent) -> Action {
        match key.code {
            // Return to input mode
            KeyCode::Char('i') | KeyCode::Enter | KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                Action::None
            }
            // Scroll up
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_up(1);
                Action::None
            }
            // Scroll down
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_down(1);
                Action::None
            }
            // Page up
            KeyCode::PageUp => {
                self.scroll_up(20);
                Action::None
            }
            // Page down
            KeyCode::PageDown => {
                self.scroll_down(20);
                Action::None
            }
            // Jump to bottom
            KeyCode::Char('G') | KeyCode::End => {
                self.scroll_offset = 0;
                self.auto_scroll = true;
                Action::None
            }
            // Jump to top
            KeyCode::Home => {
                if self.total_lines > 0 {
                    self.scroll_offset = self.total_lines;
                    self.auto_scroll = false;
                }
                Action::None
            }
            // Quit
            KeyCode::Char('q') => Action::Quit,
            _ => Action::None,
        }
    }

    fn scroll_up(&mut self, lines: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
        self.auto_scroll = false;
    }

    fn scroll_down(&mut self, lines: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
        if self.scroll_offset == 0 {
            self.auto_scroll = true;
        }
    }

    fn cycle_history_back(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let idx = match self.history_index {
            Some(i) if i > 0 => i - 1,
            Some(_) => return,
            None => self.input_history.len() - 1,
        };
        self.history_index = Some(idx);
        self.set_input_text(&self.input_history[idx].clone());
    }

    fn cycle_history_forward(&mut self) {
        match self.history_index {
            Some(i) if i + 1 < self.input_history.len() => {
                self.history_index = Some(i + 1);
                self.set_input_text(&self.input_history[i + 1].clone());
            }
            Some(_) => {
                self.history_index = None;
                self.set_input_text("");
            }
            None => {}
        }
    }

    fn set_input_text(&mut self, text: &str) {
        self.input = TextArea::from(text.lines().collect::<Vec<_>>());
        self.input
            .set_cursor_line_style(ratatui::style::Style::default());
        self.input
            .set_placeholder_text("Type a message... (Option+Enter for newline)");
    }

    // ─── Tab completion ────────────────────────────────────────────────────

    fn try_complete(&mut self, reverse: bool) {
        if let Some(ref mut state) = self.completion {
            // Already completing — cycle to next/prev candidate
            if state.candidates.is_empty() {
                return;
            }
            if reverse {
                state.index = if state.index == 0 {
                    state.candidates.len() - 1
                } else {
                    state.index - 1
                };
            } else {
                state.index = (state.index + 1) % state.candidates.len();
            }
            let candidate = state.candidates[state.index].clone();
            let full = completion_to_input(&state.kind, &candidate);
            self.set_input_text(&full);
            self.input.move_cursor(CursorMove::End);
        } else {
            self.start_completion();
        }
    }

    fn start_completion(&mut self) {
        // Only complete on single-line input
        if self.input.lines().len() > 1 {
            return;
        }
        let text = self.input.lines()[0].to_string();

        if let Some(prefix) = text.strip_prefix("/attach ") {
            // File path completion
            self.init_completion(CompletionKind::FilePath, complete_file_paths(prefix));
        } else if let Some(prefix) = text.strip_prefix("/switch ") {
            // Session ID completion from cache (short IDs for prefix matching)
            let candidates: Vec<String> = self
                .cached_sessions
                .iter()
                .filter(|(id, _)| id.starts_with(prefix))
                .map(|(id, _)| id[..id.len().min(12)].to_string())
                .collect();
            self.init_completion(CompletionKind::SessionId, candidates);
        } else if let Some(prefix) = text.strip_prefix("/agent ") {
            // Agent ID completion from cache
            let candidates: Vec<String> = self
                .cached_agents
                .iter()
                .filter(|(id, _)| id.starts_with(prefix))
                .map(|(id, _)| id.clone())
                .collect();
            self.init_completion(CompletionKind::AgentId, candidates);
        } else if text.starts_with('/') {
            // Slash command completion
            let candidates: Vec<String> = SLASH_COMMANDS
                .iter()
                .filter(|cmd| cmd.starts_with(text.as_str()))
                .map(|s| s.to_string())
                .collect();
            self.init_completion(CompletionKind::SlashCommand, candidates);
        }
    }

    /// Set up completion state from a list of candidates. If exactly one match,
    /// apply it immediately without entering cycling mode.
    fn init_completion(&mut self, kind: CompletionKind, candidates: Vec<String>) {
        if candidates.is_empty() {
            return;
        }
        if candidates.len() == 1 {
            let full = completion_to_input(&kind, &candidates[0]);
            self.set_input_text(&full);
            self.input.move_cursor(CursorMove::End);
            return;
        }
        self.completion = Some(CompletionState {
            kind: kind.clone(),
            candidates: candidates.clone(),
            index: 0,
        });
        // Apply the first candidate
        let full = completion_to_input(&kind, &candidates[0]);
        self.set_input_text(&full);
        self.input.move_cursor(CursorMove::End);
    }

    // ─── Gateway events ──────────────────────────────────────────────────

    fn handle_gateway_event(&mut self, event: GatewayEvent) -> Action {
        match event {
            GatewayEvent::Connected => {
                self.connection_status = ConnectionStatus::Connected;
                Action::None
            }
            GatewayEvent::Message(msg) => {
                // Session isolation: only accept messages for our session.
                // Adopt session_id from the first response (when we have none).
                if let Some(ref sid) = msg.session_id {
                    if !sid.is_empty() {
                        if let Some(ref my_sid) = self.session_id {
                            if sid != my_sid {
                                return Action::None; // Not our session
                            }
                        } else {
                            // First response — adopt this session
                            self.session_id = Some(sid.clone());
                        }
                    }
                }

                let action = Action::None;

                match msg.kind() {
                    ServerMessageKind::Message if !self.stream_cancelled => {
                        if let Some(text) = msg.text() {
                            self.push_message(MessageRole::Assistant, text.to_string());
                        }
                        self.waiting_for_response = false;
                        self.message_count += 1;
                    }
                    ServerMessageKind::Message => {} // ignore after cancel
                    ServerMessageKind::Image => {
                        // The terminal can't render images; show the caption (if
                        // any) plus a marker so the user knows one was produced.
                        let caption = msg.text().unwrap_or("");
                        let note = if caption.is_empty() {
                            "[image generated — view in the app]".to_string()
                        } else {
                            format!("{caption}\n[image generated — view in the app]")
                        };
                        self.push_message(MessageRole::Assistant, note);
                        self.waiting_for_response = false;
                        self.message_count += 1;
                    }
                    ServerMessageKind::StreamStart if self.stream_cancelled => {
                        // Only accept a new stream if we've sent a new message
                        if self.pending_new_request {
                            self.stream_cancelled = false;
                            self.pending_new_request = false;
                            self.streaming = true;
                            self.stream_buffer.clear();
                            self.messages.push(ChatMessage {
                                role: MessageRole::Assistant,
                                content: String::new(),
                                tools: Vec::new(),
                                timestamp: Utc::now(),
                                rendered: None,
                            });
                        }
                        // Otherwise ignore — this is from the old cancelled task
                    }
                    ServerMessageKind::StreamStart => {
                        self.compacting = false;
                        self.streaming = true;
                        self.stream_buffer.clear();
                        // Reuse existing empty assistant message (created by pre-stream ToolStart)
                        let reuse = self.messages.last().is_some_and(|m| {
                            m.role == MessageRole::Assistant && m.content.is_empty()
                        });
                        if !reuse {
                            self.messages.push(ChatMessage {
                                role: MessageRole::Assistant,
                                content: String::new(),
                                tools: Vec::new(),
                                timestamp: Utc::now(),
                                rendered: None,
                            });
                        }
                    }
                    ServerMessageKind::StreamDelta if !self.stream_cancelled => {
                        if let Some(text) = msg.text() {
                            self.stream_buffer.push_str(text);
                            if let Some(last) = self.messages.last_mut() {
                                last.content = self.stream_buffer.clone();
                                last.rendered = None; // invalidate cache
                            }
                            if self.auto_scroll {
                                self.scroll_offset = 0;
                            }
                        }
                    }
                    ServerMessageKind::StreamEnd if !self.stream_cancelled => {
                        self.streaming = false;
                        self.waiting_for_response = false;
                        self.message_count += 1;
                        // Final invalidation for complete markdown rendering
                        if let Some(last) = self.messages.last_mut() {
                            last.rendered = None;
                        }
                    }
                    ServerMessageKind::StreamDiscard if !self.stream_cancelled => {
                        if let Some((len, reused)) = self.call_anchor {
                            self.messages.truncate(len);
                            if let Some(last) = self.messages.last_mut().filter(|_| reused) {
                                last.content.clear();
                                last.rendered = None;
                            }
                        }
                        self.stream_buffer.clear();
                        self.streaming = false;
                        self.waiting_for_response = true;
                    }
                    // Ignore stream events after cancellation
                    ServerMessageKind::StreamDelta
                    | ServerMessageKind::StreamEnd
                    | ServerMessageKind::StreamDiscard => {}
                    // Typing is implied by waiting_for_response; it opens a model call.
                    ServerMessageKind::Typing => {
                        self.call_anchor = Some((
                            self.messages.len(),
                            self.messages.last().is_some_and(|m| {
                                m.role == MessageRole::Assistant && m.content.is_empty()
                            }),
                        ));
                    }
                    ServerMessageKind::ToolStart if !self.stream_cancelled => {
                        if let Some(text) = msg.text() {
                            let tool = ToolActivity {
                                name: text.to_string(),
                                done: false,
                                detail: String::new(),
                            };
                            // Attach to current assistant message, or create one
                            let needs_new = self
                                .messages
                                .last()
                                .is_none_or(|m| m.role != MessageRole::Assistant);
                            if needs_new {
                                self.messages.push(ChatMessage {
                                    role: MessageRole::Assistant,
                                    content: String::new(),
                                    tools: Vec::new(),
                                    timestamp: Utc::now(),
                                    rendered: None,
                                });
                            }
                            if let Some(last) = self.messages.last_mut() {
                                last.tools.push(tool);
                                last.rendered = None;
                            }
                        }
                    }
                    ServerMessageKind::ToolEnd if !self.stream_cancelled => {
                        if let Some(text) = msg.text() {
                            if let Some(last) = self.messages.last_mut() {
                                if last.role == MessageRole::Assistant {
                                    // Mark last running tool as done
                                    if let Some(tool) =
                                        last.tools.iter_mut().rev().find(|t| !t.done)
                                    {
                                        tool.done = true;
                                        tool.detail = text.to_string();
                                    }
                                    last.rendered = None;
                                }
                            }
                        }
                    }
                    ServerMessageKind::ToolStart | ServerMessageKind::ToolEnd => {}
                    ServerMessageKind::Error => {
                        self.compacting = false;
                        let err = msg
                            .error
                            .as_deref()
                            .or(msg.text())
                            .unwrap_or("unknown error");
                        self.push_message(MessageRole::System, format!("Error: {}", err));
                        self.waiting_for_response = false;
                    }
                    ServerMessageKind::Reminder => {
                        if let Some(text) = msg.text() {
                            self.push_message(MessageRole::System, text.to_string());
                        }
                    }
                    ServerMessageKind::CompactStart => {
                        self.compacting = true;
                    }
                    ServerMessageKind::CompactEnd => {
                        self.compacting = false;
                    }
                    ServerMessageKind::Reaction => {
                        // Reactions are handled by the UI layer — no TUI display needed
                    }
                    ServerMessageKind::ContextUpdate => {
                        if let Some(tokens) = msg.context_tokens {
                            self.context_tokens = tokens;
                        }
                        if let Some(max) = msg.context_max {
                            self.context_max = max;
                        }
                    }
                    ServerMessageKind::PermissionPrompt => {
                        if let (Some(rid), Some(tn)) =
                            (msg.request_id.as_deref(), msg.tool_name.as_deref())
                        {
                            let reason = msg.text().unwrap_or("");
                            self.push_message(
                                MessageRole::System,
                                format!(
                                    "🔐 {tn} needs approval ({rid}) — {reason}. Reply with `approve {rid}` or `reject {rid}`.",
                                ),
                            );
                        }
                    }
                    ServerMessageKind::Pong
                    | ServerMessageKind::Connected
                    | ServerMessageKind::NodeInvoke
                    | ServerMessageKind::TeamUpdate
                    | ServerMessageKind::TurnIndicator
                    | ServerMessageKind::LocalExec
                    | ServerMessageKind::LocalExecResult
                    | ServerMessageKind::ConnectPrompt
                    | ServerMessageKind::Route
                    | ServerMessageKind::Unknown => {}
                }

                // Update context from any message that carries token data
                if let Some(tokens) = msg.context_tokens {
                    self.context_tokens = tokens;
                }
                if let Some(max) = msg.context_max {
                    self.context_max = max;
                }

                action
            }
            GatewayEvent::Disconnected { reason } => {
                self.connection_status = ConnectionStatus::Disconnected;
                self.push_message(MessageRole::System, format!("Disconnected: {}", reason));
                Action::None
            }
            GatewayEvent::Reconnecting { attempt } => {
                self.connection_status = ConnectionStatus::Reconnecting(attempt);
                Action::None
            }
            GatewayEvent::ReconnectFailed => {
                self.connection_status = ConnectionStatus::Disconnected;
                self.push_message(
                    MessageRole::System,
                    "Failed to reconnect to gateway".to_string(),
                );
                Action::None
            }
        }
    }

    // ─── Stream cancellation ─────────────────────────────────────────────

    pub fn cancel_stream(&mut self) {
        self.streaming = false;
        self.waiting_for_response = false;
        self.stream_cancelled = true;

        // Append stop indicator to the last assistant message
        if let Some(last) = self.messages.last_mut() {
            if last.role == MessageRole::Assistant {
                last.content.push_str(" ⏹");
                last.rendered = None;
            }
        }
    }

    // ─── Message helpers ─────────────────────────────────────────────────

    pub fn push_message(&mut self, role: MessageRole, content: String) {
        self.messages.push(ChatMessage {
            role,
            content,
            tools: Vec::new(),
            timestamp: Utc::now(),
            rendered: None,
        });
        if self.auto_scroll {
            self.scroll_offset = 0;
        }
    }

    pub fn add_user_message(&mut self, text: &str) {
        self.pending_new_request = true;
        self.push_message(MessageRole::User, text.to_string());
        self.waiting_for_response = true;
        self.message_count += 1;
    }

    /// Load transcript entries from the server into the message list.
    ///
    /// Maps user/assistant entries to chat messages and attaches tool entries
    /// as completed tool indicators on the preceding assistant message.
    pub fn load_transcript(&mut self, entries: &[TranscriptEntryDto]) {
        self.messages.clear();
        self.call_anchor = None;
        self.message_count = 0;

        for entry in entries {
            match entry.role.as_str() {
                "user" => {
                    self.messages.push(ChatMessage {
                        role: MessageRole::User,
                        content: entry.content.clone(),
                        tools: Vec::new(),
                        timestamp: Utc::now(),
                        rendered: None,
                    });
                    self.message_count += 1;
                }
                "assistant" => {
                    self.messages.push(ChatMessage {
                        role: MessageRole::Assistant,
                        content: entry.content.clone(),
                        tools: Vec::new(),
                        timestamp: Utc::now(),
                        rendered: None,
                    });
                    self.message_count += 1;
                }
                "tool" => {
                    // Attach to last assistant message as a completed tool indicator
                    if let Some(last) = self.messages.last_mut() {
                        if last.role == MessageRole::Assistant {
                            let name = entry
                                .tool_metadata
                                .as_ref()
                                .map(|t| t.name.clone())
                                .unwrap_or_default();
                            let success = entry
                                .tool_metadata
                                .as_ref()
                                .map(|t| t.success)
                                .unwrap_or(true);
                            let detail = if success { "done" } else { "failed" };
                            last.tools.push(ToolActivity {
                                name,
                                done: true,
                                detail: detail.to_string(),
                            });
                        }
                    }
                }
                _ => {} // skip system messages (internal prompts)
            }
        }

        if self.auto_scroll {
            self.scroll_offset = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(msg_type: &str, content: &str) -> GatewayEvent {
        GatewayEvent::Message(
            serde_json::from_value(serde_json::json!({"type": msg_type, "content": content}))
                .unwrap(),
        )
    }

    fn contents(app: &App) -> Vec<&str> {
        app.messages.iter().map(|m| m.content.as_str()).collect()
    }

    #[test]
    fn a_discard_clears_the_live_message() {
        let mut app = App::new("m".into(), None, "ws://localhost".into());
        app.push_message(MessageRole::Assistant, "an earlier answer".into());
        [
            server("typing", ""),
            server("stream_start", ""),
            server("stream_delta", "Let me check."),
            server("stream_end", ""),
            server("stream_start", ""),
            server("stream_delta", "The abandoned"),
            server("stream_discard", "interrupted"),
            server("stream_start", ""),
            server("stream_delta", "The real answer."),
            server("stream_end", ""),
        ]
        .into_iter()
        .for_each(|event| {
            app.handle_gateway_event(event);
        });
        assert_eq!(contents(&app), ["an earlier answer", "The real answer."]);
        assert!(!app.waiting_for_response, "the real answer ended the turn");
    }

    #[test]
    fn a_discard_with_no_call_begun_keeps_the_history() {
        let mut app = App::new("m".into(), None, "ws://localhost".into());
        app.push_message(MessageRole::User, "a question".into());
        app.push_message(MessageRole::Assistant, "an earlier answer".into());
        app.handle_gateway_event(server("stream_discard", "interrupted"));
        assert_eq!(contents(&app), ["a question", "an earlier answer"]);
        assert!(app.waiting_for_response);
    }
}

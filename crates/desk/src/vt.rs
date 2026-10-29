//! The screen a terminal shows, kept by the hub: every byte a PTY writes is
//! parsed into a `vt100` model, so a viewer that attaches late (or falls
//! behind) is sent the screen as it is now — its modes, the alternate screen
//! and the primary screen's history — instead of a replay of raw bytes.
//!
//! `vt100` does not model a few modes Claude Code sets at startup (focus
//! events, colour-scheme notifications, the kitty keyboard flags,
//! modifyOtherKeys); they are tracked here and put back in every snapshot.
//! Queries a terminal must answer are answered as Desk's xterm.js 6 does,
//! for when no viewer is there to answer them.

use std::num::NonZeroU16;

use vt100::{Callbacks, MouseProtocolEncoding, MouseProtocolMode, Screen};

/// Most kitty keyboard flag pushes remembered.
const KITTY_DEPTH: usize = 16;

/// What `vt100` leaves to its callbacks: modes it doesn't model, the window
/// title, the shell's working directory (OSC 7), and the replies owed for
/// queries in the output just parsed.
#[derive(Default)]
struct Tracker {
    replies: Vec<u8>,
    focus_events: bool,
    color_scheme_events: bool,
    synchronized: bool,
    kitty: Vec<u16>,
    modify_other_keys: Option<u16>,
    title: Option<Vec<u8>>,
    cwd: Option<String>,
}

fn first(params: &[&[u16]], i: usize) -> Option<u16> {
    params.get(i).and_then(|p| p.first().copied())
}

/// DECRQM's answer for a private mode, as xterm.js 6 gives it: 1 set, 2
/// reset, 4 permanently reset, 0 not recognised.
fn private_mode(t: &Tracker, s: &Screen, mode: u16) -> u8 {
    let on = |b: bool| if b { 1 } else { 2 };
    match mode {
        1 => on(s.application_cursor()),
        25 => on(!s.hide_cursor()),
        66 => on(s.application_keypad()),
        9 => on(s.mouse_protocol_mode() == MouseProtocolMode::Press),
        1000 => on(s.mouse_protocol_mode() == MouseProtocolMode::PressRelease),
        1002 => on(s.mouse_protocol_mode() == MouseProtocolMode::ButtonMotion),
        1003 => on(s.mouse_protocol_mode() == MouseProtocolMode::AnyMotion),
        1004 => on(t.focus_events),
        1005 | 1015 => 4,
        1006 => on(s.mouse_protocol_encoding() == MouseProtocolEncoding::Sgr),
        47 | 1047 | 1049 => on(s.alternate_screen()),
        2004 => on(s.bracketed_paste()),
        2026 => on(t.synchronized),
        _ => 0,
    }
}

impl Callbacks for Tracker {
    fn set_window_title(&mut self, _: &mut Screen, title: &[u8]) {
        self.title = Some(title.to_vec());
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        match (i1, i2, c) {
            // Private modes vt100 leaves alone. `decset` calls this once per
            // mode it doesn't know, with all of them, so setting is idempotent.
            (Some(b'?'), None, 'h' | 'l') => {
                let on = c == 'h';
                for p in params.iter().filter_map(|p| p.first()) {
                    match p {
                        1004 => self.focus_events = on,
                        2031 => self.color_scheme_events = on,
                        2026 => self.synchronized = on,
                        _ => {}
                    }
                }
            }
            // DA1
            (None, None, 'c') if first(params, 0).unwrap_or(0) == 0 => {
                self.replies.extend_from_slice(b"\x1b[?1;2c");
            }
            // DA2
            (Some(b'>'), None, 'c') if first(params, 0).unwrap_or(0) == 0 => {
                self.replies.extend_from_slice(b"\x1b[>0;276;0c");
            }
            // DSR: status, cursor position.
            (None, None, 'n') => match first(params, 0) {
                Some(5) => self.replies.extend_from_slice(b"\x1b[0n"),
                Some(6) => {
                    let (row, col) = screen.cursor_position();
                    self.replies
                        .extend(format!("\x1b[{};{}R", row + 1, col + 1).bytes());
                }
                _ => {}
            },
            (Some(b'?'), None, 'n') if first(params, 0) == Some(6) => {
                let (row, col) = screen.cursor_position();
                self.replies
                    .extend(format!("\x1b[?{};{}R", row + 1, col + 1).bytes());
            }
            // DECRQM
            (Some(b'?'), Some(b'$'), 'p') => {
                let mode = first(params, 0).unwrap_or(0);
                let state = private_mode(self, screen, mode);
                self.replies
                    .extend(format!("\x1b[?{mode};{state}$y").bytes());
            }
            (Some(b'$'), None, 'p') => {
                let mode = first(params, 0).unwrap_or(0);
                let state = match mode {
                    2 => 4,
                    4 | 20 => 2,
                    12 => 3,
                    _ => 0,
                };
                self.replies
                    .extend(format!("\x1b[{mode};{state}$y").bytes());
            }
            // Kitty keyboard: push, pop, set.
            (Some(b'>'), None, 'u') => {
                if self.kitty.len() == KITTY_DEPTH {
                    self.kitty.remove(0);
                }
                self.kitty.push(first(params, 0).unwrap_or(0));
            }
            (Some(b'<'), None, 'u') => {
                let n = usize::from(first(params, 0).unwrap_or(1).max(1));
                let keep = self.kitty.len().saturating_sub(n);
                self.kitty.truncate(keep);
            }
            (Some(b'='), None, 'u') => {
                let flags = first(params, 0).unwrap_or(0);
                let top = self.kitty.pop().unwrap_or(0);
                self.kitty.push(match first(params, 1).unwrap_or(1) {
                    2 => top | flags,
                    3 => top & !flags,
                    _ => flags,
                });
            }
            // modifyOtherKeys
            (Some(b'>'), None, 'm') if first(params, 0) == Some(4) => {
                self.modify_other_keys = first(params, 1).filter(|&n| n > 0);
            }
            // XTVERSION (`CSI > q`), the kitty keyboard query (`CSI ? u`) and
            // the rest go unanswered, as xterm.js leaves them: the DA1 a
            // program sends after them tells it they aren't supported.
            _ => {}
        }
    }

    fn unhandled_osc(&mut self, _: &mut Screen, params: &[&[u8]]) {
        // OSC 7 ; file://host/path — where the shell is. OSC 7501 (Claude
        // Code's status protocol query) stays unanswered, as in xterm.js.
        if let [b"7", url, ..] = params {
            if let Some(path) = file_url_path(url) {
                self.cwd = Some(path);
            }
        }
    }
}

/// The path of a `file://host/path` URL, percent-decoded.
fn file_url_path(url: &[u8]) -> Option<String> {
    let rest = url.strip_prefix(b"file://")?;
    let path = &rest[rest.iter().position(|&b| b == b'/')?..];
    let mut out = Vec::with_capacity(path.len());
    let mut i = 0;
    while i < path.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (path[i], path.get(i + 1).copied(), path.get(i + 2).copied()) {
            (b'%', Some(h), Some(l)) if hex(h).is_some() && hex(l).is_some() => {
                out.push((hex(h)? * 16 + hex(l)?) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n.max(2)).expect("at least 2")
}

/// Mode resets that put a viewer of unknown state back to the defaults the
/// snapshot's own mode settings are written against.
const MODE_RESETS: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\
\x1b[?1004l\x1b[?2031l\x1b[?2004l\x1b[?1l\x1b>\x1b[?25h\x1b[<16u\x1b[>4m";

pub struct Term {
    parser: vt100::Parser<Tracker>,
}

impl Term {
    pub fn new(cols: u16, rows: u16, scrollback_rows: usize) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                nz(rows),
                nz(cols),
                scrollback_rows,
                Tracker::default(),
            ),
        }
    }

    /// Parses `bytes`; returns the replies owed to queries in them, in order.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.process(bytes);
        std::mem::take(&mut self.parser.callbacks_mut().replies)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.set_size(nz(rows), nz(cols));
    }

    /// (cols, rows)
    pub fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.parser.screen().size();
        (cols.get(), rows.get())
    }

    /// The working directory the shell last reported (OSC 7).
    pub fn cwd(&self) -> Option<&str> {
        self.parser.callbacks().cwd.as_deref()
    }

    /// Escape sequences that bring a terminal to this screen. With
    /// `history`, for a fresh viewer: the primary screen's scrollback comes
    /// first. Without, for one that fell behind: only the screen, so its
    /// history isn't written twice. Either way it starts by cancelling any
    /// escape sequence the viewer's parser was in the middle of.
    pub fn snapshot(&self, history: bool) -> Vec<u8> {
        let screen = self.parser.screen();
        let mut out = String::from("\x18");
        out.push_str(std::str::from_utf8(MODE_RESETS).expect("ascii"));
        if screen.alternate_screen() {
            // The primary screen as it will be when the program leaves the
            // alternate one: a copy, switched back.
            let mut primary = vt100::Parser::new(nz(1), nz(1), 0);
            *primary.screen_mut() = screen.clone();
            primary.process(b"\x1b[?1049l");
            if history {
                push_history(&mut out, primary.screen());
            } else {
                out.push_str("\x1b[?1049l");
            }
            out.push_str(&primary.screen().contents_formatted());
            out.push_str("\x1b[?1049h");
        } else if history {
            push_history(&mut out, screen);
        } else {
            out.push_str("\x1b[?1049l");
        }
        out.push_str(&screen.state_formatted());
        let t = self.parser.callbacks();
        if t.focus_events {
            out.push_str("\x1b[?1004h");
        }
        if t.color_scheme_events {
            out.push_str("\x1b[?2031h");
        }
        for flags in &t.kitty {
            out.push_str(&format!("\x1b[>{flags}u"));
        }
        if let Some(n) = t.modify_other_keys {
            out.push_str(&format!("\x1b[>4;{n}m"));
        }
        let mut bytes = out.into_bytes();
        if let Some(title) = &t.title {
            bytes.extend_from_slice(b"\x1b]2;");
            bytes.extend_from_slice(title);
            bytes.push(0x07);
        }
        bytes
    }

    /// What the terminal shows as plain text: the primary screen's history
    /// and screen, or the alternate screen while a program has it.
    pub fn text(&self) -> String {
        let screen = self.parser.screen();
        if screen.alternate_screen() {
            return screen.contents();
        }
        let mut basic = String::new();
        let mut state = vt100::capture::BasicFormattedCaptureState::new();
        let _ = screen.write_contents_formatted_basic(
            &mut basic,
            vt100::capture::BasicFormattedCaptureRange::Full(&mut state),
        );
        vt100::capture::basic_formatted_to_plain(&basic).collect()
    }
}

/// The screen's scrollback and rows, one line each, ending with the rows
/// on the viewer's screen ready to be drawn over exactly.
fn push_history(out: &mut String, screen: &Screen) {
    let mut basic = String::new();
    let mut state = vt100::capture::BasicFormattedCaptureState::new();
    let _ = screen.write_contents_formatted_basic(
        &mut basic,
        vt100::capture::BasicFormattedCaptureRange::Full(&mut state),
    );
    out.push_str("\x1b[m");
    out.push_str(&basic.replace('\n', "\r\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh viewer's screen after `bytes`, as a second model.
    fn replay(t: &Term, history: bool) -> vt100::Parser<Tracker> {
        let (cols, rows) = t.size();
        let mut viewer =
            vt100::Parser::new_with_callbacks(nz(rows), nz(cols), 100, Tracker::default());
        viewer.process(&t.snapshot(history));
        viewer
    }

    /// Claude Code's startup: modes, then its queries ending in DA1.
    const CLAUDE_START: &[u8] = b"\x1b[?2004h\x1b[?2031h\x1b[?1004h\x1b[>5u\x1b[>4;2m\
\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?2026h\
\x1b[>q\x1b[?u\x1b]7501;?\x07\x1b[c";

    #[test]
    fn claude_codes_startup_is_answered_as_xterm_js_answers_it() {
        let mut t = Term::new(80, 24, 100);
        // Split mid-sequence: the parser carries state across chunks.
        let (a, b) = CLAUDE_START.split_at(CLAUDE_START.len() - 2);
        assert!(t.feed(a).is_empty());
        assert_eq!(t.feed(b), b"\x1b[?1;2c");
        assert_eq!(
            t.feed(b"ab\x1b[6n\x1b[?2026$p\x1b[?1049$p\x1b[?7727$p"),
            b"\x1b[1;3R\x1b[?2026;1$y\x1b[?1049;1$y\x1b[?7727;0$y"
        );
    }

    #[test]
    fn attaching_mid_session_restores_the_modes_and_the_alternate_screen() {
        let mut t = Term::new(80, 24, 100);
        t.feed(b"$ claude\r\n");
        t.feed(CLAUDE_START);
        t.feed(b"\x1b]0;\xe2\x9c\xb3 auth-fix\x07\x1b[2J\x1b[5;3H\x1b[1mWelcome\x1b[m\x1b[?2026l");
        let snap = t.snapshot(true);
        let text = String::from_utf8_lossy(&snap);
        for mode in [
            "\x1b[?1004h",
            "\x1b[?2031h",
            "\x1b[>5u",
            "\x1b[>4;2m",
            "\x1b[?1049h",
        ] {
            assert!(text.contains(mode), "snapshot lacks {mode:?}");
        }
        assert!(
            !text.contains("\x1b[?2026h"),
            "synchronized output is not replayed"
        );
        assert!(text.contains("\x1b]2;\u{2733} auth-fix\x07"));

        let viewer = replay(&t, true);
        let s = viewer.screen();
        assert!(s.alternate_screen());
        assert!(s.bracketed_paste());
        assert_eq!(s.mouse_protocol_mode(), MouseProtocolMode::AnyMotion);
        assert_eq!(s.mouse_protocol_encoding(), MouseProtocolEncoding::Sgr);
        assert_eq!(s.contents(), t.parser.screen().contents());
        assert_eq!(s.cursor_position(), (4, 9));
        assert!(s.cell(4, 2).unwrap().bold());
        let vt = viewer.callbacks();
        assert!(vt.focus_events && vt.color_scheme_events);
        assert_eq!(
            (vt.kitty.as_slice(), vt.modify_other_keys),
            (&[5][..], Some(2))
        );

        // Leaving the alternate screen shows what was under it.
        let mut viewer = viewer;
        viewer.process(b"\x1b[?1049l");
        assert!(viewer.screen().contents().starts_with("$ claude"));
    }

    #[test]
    fn a_fresh_viewer_gets_the_primary_history_and_a_resync_does_not() {
        let mut t = Term::new(20, 5, 100);
        for i in 0..12 {
            t.feed(format!("line {i}\r\n").as_bytes());
        }
        t.feed(b"\x1b[31mred\x1b[m $ ");
        let mut viewer = replay(&t, true);
        assert_eq!(viewer.screen().contents(), t.parser.screen().contents());
        assert_eq!(
            viewer.screen().cursor_position(),
            t.parser.screen().cursor_position()
        );
        viewer.screen_mut().set_scrollback(100);
        assert!(viewer.screen().contents().starts_with("line 0\n"));
        let resync = String::from_utf8(t.snapshot(false)).unwrap();
        assert!(resync.starts_with('\x18'));
        assert!(!resync.contains("line 0"));
        assert!(t.text().starts_with("line 0\nline 1\n"));
    }

    #[test]
    fn a_resync_cancels_a_half_written_sequence_and_resets_stale_modes() {
        let mut t = Term::new(40, 6, 0);
        t.feed(b"\x1b[?1000hhello");
        t.feed(b"\x1b[?1000l");
        let (cols, rows) = t.size();
        let mut viewer =
            vt100::Parser::new_with_callbacks(nz(rows), nz(cols), 0, Tracker::default());
        // The viewer saw mouse mode go on, then lost bytes mid-sequence.
        viewer.process(b"\x1b[?1000hhello\x1b[3");
        viewer.process(&t.snapshot(false));
        assert_eq!(
            viewer.screen().mouse_protocol_mode(),
            MouseProtocolMode::None
        );
        assert_eq!(viewer.screen().contents(), "hello");
    }

    #[test]
    fn kitty_flags_push_pop_and_set_and_osc_7_reports_the_cwd() {
        let mut t = Term::new(40, 6, 0);
        t.feed(b"\x1b[>1u\x1b[>3u\x1b[<u\x1b[=4;2u");
        assert_eq!(t.parser.callbacks().kitty, vec![5]);
        t.feed(b"\x1b]7;file://mac.local/Users/k/dev/my%20repo\x07");
        assert_eq!(t.cwd(), Some("/Users/k/dev/my repo"));
        t.feed(b"\x1b]7;file:///tmp\x1b\\");
        assert_eq!(t.cwd(), Some("/tmp"));
    }
}

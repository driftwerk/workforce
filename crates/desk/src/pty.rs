//! A CLI running in a pseudo-terminal. Every byte it writes is parsed into
//! the hub's model of its screen ([`crate::vt::Term`]) and sent on to every
//! attached viewer, under one lock, so a viewer that attaches (or falls
//! behind) is sent the screen as it is and then every byte after it.
//!
//! One viewer at a time is the terminal's *active* viewer: its keystrokes,
//! its size and its answers to the program's queries are the ones that
//! count; every other viewer mirrors it, read-only, at its size. While no
//! viewer is active the hub answers the program's queries itself.
//!
//! The process is either the hub's own child ([`Pty::spawn`]) or one a PTY
//! holder keeps across hub restarts ([`crate::holder`]); viewers can't tell.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use eyre::{Context, Result};
use parking_lot::Mutex;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot};

use crate::vt::Term;

pub const DEFAULT_COLS: u16 = 120;
pub const DEFAULT_ROWS: u16 = 40;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

/// Its environment's names only: a session's may hold its account's token.
impl std::fmt::Debug for Spawn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spawn")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// How the process ended: its exit code, or `None` when it could not be read.
pub type Exit = Option<u32>;

/// Screen rows of history kept for `bytes` of configured scrollback: about
/// 2 KB a row, from 100 to 10 000 rows.
pub fn history_rows(bytes: usize) -> usize {
    (bytes / 2048).clamp(100, 10_000)
}

/// What goes out to viewers, in order.
#[derive(Debug, Clone)]
pub enum Frame {
    /// Terminal output, or a snapshot of the screen.
    Bytes(Bytes),
    /// The terminal's size changed (or is this, on attach).
    Size { cols: u16, rows: u16 },
    /// Which viewer is active, if any.
    Active(Option<u64>),
    /// The process ended.
    Exit,
}

/// Where keystrokes, sizes and kills go: the hub's own child, or a holder's.
pub(crate) trait Io: Send + Sync {
    fn write(&self, input: &[u8]) -> Result<()>;
    fn resize(&self, cols: u16, rows: u16) -> Result<()>;
    fn kill(&self);
}

struct Core {
    term: Term,
    /// What its colour queries are answered from, kept across a reset.
    palette: Option<crate::vt::Palette>,
    /// No one is ever shown it: it answers every query its program waits on
    /// ([`Pty::spawn_unseen`]), kept across a reset.
    unseen: bool,
    active: Option<u64>,
    /// Attached viewers, oldest first, with the size each last asked for.
    viewers: Vec<(u64, Option<(u16, u16)>)>,
    next_viewer: u64,
    exited: bool,
}

impl Core {
    /// Makes `id` the active viewer; the size it asked for, to apply.
    fn activate(&mut self, id: u64, tx: &broadcast::Sender<Frame>) -> Option<(u16, u16)> {
        if self.active != Some(id) {
            self.active = Some(id);
            let _ = tx.send(Frame::Active(Some(id)));
        }
        self.viewers
            .iter()
            .find(|(v, _)| *v == id)
            .and_then(|(_, size)| *size)
            .filter(|&size| size != self.term.size())
    }
}

/// The screen model and the viewers' channel, shared with whatever reads
/// the process's output.
pub(crate) struct Screen {
    core: Mutex<Core>,
    tx: broadcast::Sender<Frame>,
    /// Output is dropped unread, never parsed or sent on: a terminal that
    /// may be echoing a secret typed into it ([`Pty::blind`]).
    blind: std::sync::atomic::AtomicBool,
    /// The hub's own child has been waited for: its pid may be another's.
    reaped: Mutex<Option<std::time::Instant>>,
    /// While blind, the start of a query the last output ended in, to be
    /// answered once the rest of it comes: never more than a query's own
    /// bytes, none of the program's text.
    blind_tail: Mutex<Vec<u8>>,
}

/// The term a screen draws with.
fn term(
    cols: u16,
    rows: u16,
    history: usize,
    palette: Option<crate::vt::Palette>,
    unseen: bool,
) -> Term {
    let term = Term::with_palette(cols, rows, history, palette);
    match unseen {
        true => term.answering_kitty(),
        false => term,
    }
}

impl Screen {
    pub(crate) fn new(
        cols: u16,
        rows: u16,
        history: usize,
        palette: Option<crate::vt::Palette>,
    ) -> Arc<Self> {
        Self::with(cols, rows, history, palette, false)
    }

    /// [`Self::new`]; `unseen`, one no one is ever shown ([`Pty::spawn_unseen`]).
    fn with(
        cols: u16,
        rows: u16,
        history: usize,
        palette: Option<crate::vt::Palette>,
        unseen: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            core: Mutex::new(Core {
                term: term(cols, rows, history, palette.clone(), unseen),
                palette,
                unseen,
                active: None,
                viewers: Vec::new(),
                next_viewer: 1,
                exited: false,
            }),
            tx: broadcast::channel(1024).0,
            blind: Default::default(),
            reaped: Default::default(),
            blind_tail: Default::default(),
        })
    }

    /// Takes in output. Returns the replies the hub owes the program's
    /// queries when no viewer is active to answer them.
    pub(crate) fn push(&self, chunk: &[u8]) -> Option<Vec<u8>> {
        if self.blind.load(std::sync::atomic::Ordering::SeqCst) {
            // Unread, but a program waiting on the terminal's answer to a
            // cursor or device query isn't left hanging: fixed answers,
            // nothing of the output taken in.
            // A viewer never sees blind output, so it can't answer: the
            // hub always does.
            let replies = blind_replies(&mut self.blind_tail.lock(), chunk);
            return (!replies.is_empty()).then_some(replies);
        }
        let mut core = self.core.lock();
        let replies = core.term.feed(chunk);
        let _ = self.tx.send(Frame::Bytes(Bytes::copy_from_slice(chunk)));
        (core.active.is_none() && !replies.is_empty()).then_some(replies)
    }

    /// Starts over from a snapshot of the screen (a holder's, after the
    /// hub reconnected or fell behind it); viewers are resynced.
    pub(crate) fn reset(&self, cols: u16, rows: u16, history: usize, snapshot: &[u8]) {
        let mut core = self.core.lock();
        let mut term = term(cols, rows, history, core.palette.clone(), core.unseen);
        term.feed(snapshot);
        core.term = term;
        let _ = self.tx.send(Frame::Size { cols, rows });
        let _ = self
            .tx
            .send(Frame::Bytes(Bytes::from(core.term.snapshot(false))));
    }

    /// A snapshot and every frame after it, taken together.
    pub(crate) fn snapshot_and_subscribe(
        &self,
        history: bool,
    ) -> (u16, u16, Vec<u8>, broadcast::Receiver<Frame>) {
        let core = self.core.lock();
        let (cols, rows) = core.term.size();
        (cols, rows, core.term.snapshot(history), self.tx.subscribe())
    }

    fn resized(&self, cols: u16, rows: u16) {
        let mut core = self.core.lock();
        core.term.resize(cols, rows);
        let _ = self.tx.send(Frame::Size { cols, rows });
    }

    pub(crate) fn exited(&self) {
        let mut core = self.core.lock();
        core.exited = true;
        let _ = self.tx.send(Frame::Exit);
    }

    pub(crate) fn snapshot(&self, history: bool) -> (u16, u16, Vec<u8>) {
        let core = self.core.lock();
        let (cols, rows) = core.term.size();
        (cols, rows, core.term.snapshot(history))
    }

    fn cwd(&self) -> Option<String> {
        self.core.lock().term.cwd().map(str::to_string)
    }

    fn text(&self) -> String {
        self.core.lock().term.text()
    }
}

/// The queries a blind screen answers, and how: the cursor position
/// (`ESC[6n`, always the top left), the device attributes (`ESC[c`,
/// `ESC[0c`, as [`Term`] answers them), the foreground and background
/// colours (OSC 10 and 11, ended by BEL or ST: white on black) and the kitty
/// keyboard flags (`ESC[?u`: none). Nothing else is answered.
const BLIND_ASKED: [(&[u8], &[u8]); 9] = [
    (b"\x1b[6n", b"\x1b[1;1R"),
    (b"\x1b[c", b"\x1b[?1;2c"),
    (b"\x1b[0c", b"\x1b[?1;2c"),
    (b"\x1b[?u", b"\x1b[?0u"),
    (b"\x1b]10;?\x07", BLIND_FOREGROUND),
    (b"\x1b]10;?\x1b\\", BLIND_FOREGROUND),
    (b"\x1b]11;?\x07", BLIND_BACKGROUND),
    (b"\x1b]11;?\x1b\\", BLIND_BACKGROUND),
    // Both at once, as xterm reads `10;?;?`.
    (
        b"\x1b]10;?;?\x07",
        b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\",
    ),
];
const BLIND_FOREGROUND: &[u8] = b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\";
const BLIND_BACKGROUND: &[u8] = b"\x1b]11;rgb:0000/0000/0000\x1b\\";

/// What a blind screen answers in `chunk` ([`BLIND_ASKED`]), in the order
/// asked. A query cut off at its end is kept in `tail` (only ever a query's
/// own first bytes) and answered once the next chunk completes it.
fn blind_replies(tail: &mut Vec<u8>, chunk: &[u8]) -> Vec<u8> {
    let joined;
    let bytes = match tail.is_empty() {
        true => chunk,
        false => {
            joined = [std::mem::take(tail).as_slice(), chunk].concat();
            &joined[..]
        }
    };
    let mut replies = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        match BLIND_ASKED.iter().find(|(q, _)| rest.starts_with(q)) {
            Some((q, reply)) => {
                replies.extend_from_slice(reply);
                at += q.len();
            }
            None if rest[0] == 0x1b
                && BLIND_ASKED
                    .iter()
                    .any(|(q, _)| q.len() > rest.len() && q.starts_with(rest)) =>
            {
                *tail = rest.to_vec();
                break;
            }
            None => at += 1,
        }
    }
    replies
}

/// One terminal.
pub struct Pty {
    screen: Arc<Screen>,
    io: Box<dyn Io>,
    pid: Option<u32>,
    /// When the person last typed in it (ms since the epoch; 0 never):
    /// Desk's line holds a paste while they do.
    typed: std::sync::atomic::AtomicI64,
}

/// Whether a viewer's input is the person's keys, not the terminal's own
/// answer to a query (a cursor report, colours), which comes as one escape
/// sequence. Esc alone is a key.
fn typed(bytes: &[u8]) -> bool {
    !bytes.is_empty() && (bytes == b"\x1b" || !bytes.starts_with(b"\x1b"))
}

/// One viewer of a [`Pty`]; dropping it detaches it, giving up the active
/// role if it held it.
pub struct Viewer {
    id: u64,
    pty: Arc<Pty>,
}

impl Pty {
    pub(crate) fn from_parts(screen: Arc<Screen>, io: Box<dyn Io>, pid: Option<u32>) -> Self {
        Self {
            screen,
            io,
            pid,
            typed: Default::default(),
        }
    }

    /// Starts `spec` as the hub's own child: it ends when this does.
    pub fn spawn(
        spec: &Spawn,
        cols: u16,
        rows: u16,
        history: usize,
    ) -> Result<(Self, oneshot::Receiver<Exit>)> {
        Self::spawn_with(spec, cols, rows, history, None)
    }

    /// [`Self::spawn`], answering colour queries from `palette`.
    pub fn spawn_with(
        spec: &Spawn,
        cols: u16,
        rows: u16,
        history: usize,
        palette: Option<crate::vt::Palette>,
    ) -> Result<(Self, oneshot::Receiver<Exit>)> {
        let (local, screen, exit) = Local::spawn(spec, cols, rows, history, palette, false)?;
        let pid = local.pid;
        Ok((Self::from_parts(screen, Box::new(local), pid), exit))
    }

    /// [`Self::spawn`] for a terminal no one is ever shown (an engine's own
    /// sign-in): no viewer will answer its program's queries, so the hub
    /// answers every one it waits on itself, seen or blind. Its colours
    /// are white on black (OSC 10, 11), and it has no kitty keyboard flags
    /// (`CSI ? u`), so the program reads plain keys from it.
    pub fn spawn_unseen(
        spec: &Spawn,
        cols: u16,
        rows: u16,
        history: usize,
    ) -> Result<(Self, oneshot::Receiver<Exit>)> {
        let palette = crate::vt::Palette::default();
        palette.set(crate::vt::Colors::white_on_black());
        let (local, screen, exit) = Local::spawn(spec, cols, rows, history, Some(palette), true)?;
        let pid = local.pid;
        Ok((Self::from_parts(screen, Box::new(local), pid), exit))
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub(crate) fn screen(&self) -> &Arc<Screen> {
        &self.screen
    }

    /// Attaches a viewer: the frames that bring it up to date (size, who
    /// is active, the screen with its history) and every frame after them.
    /// The first viewer to attach while no one is active becomes active.
    pub fn attach(self: &Arc<Self>) -> (Viewer, Vec<Frame>, broadcast::Receiver<Frame>) {
        let mut core = self.screen.core.lock();
        let id = core.next_viewer;
        core.next_viewer += 1;
        core.viewers.push((id, None));
        if core.active.is_none() && !core.exited {
            core.active = Some(id);
            let _ = self.screen.tx.send(Frame::Active(Some(id)));
        }
        let rx = self.screen.tx.subscribe();
        let (cols, rows) = core.term.size();
        let frames = vec![
            Frame::Size { cols, rows },
            Frame::Active(core.active),
            Frame::Bytes(Bytes::from(core.term.snapshot(true))),
        ];
        drop(core);
        (
            Viewer {
                id,
                pty: Arc::clone(self),
            },
            frames,
            rx,
        )
    }

    /// Tells the program the terminal turned light or dark (`CSI ? 997 ; 1
    /// n` dark, `2` light), when it asked to be told (mode 2031): it asks for
    /// the colours again. In a terminal whose process is its `shell`, only
    /// while another program is in the foreground: a program that asked and
    /// then crashed leaves the shell at its prompt, which would take the
    /// notice as typing. Whether it was told.
    pub fn tell_ground(&self, ground: crate::vt::Ground, shell: bool) -> bool {
        if !self.screen.core.lock().term.color_scheme_events() {
            return false;
        }
        if shell && !self.shell_in_background() {
            return false;
        }
        let n = match ground {
            crate::vt::Ground::Dark => 1,
            crate::vt::Ground::Light => 2,
        };
        self.io.write(format!("\x1b[?997;{n}n").as_bytes()).is_ok()
    }

    /// Whether the terminal's foreground process group is another than its
    /// process's own (a shell's, running a program). Read with `ps`, which
    /// works the same for a process the PTY holder keeps; false when it
    /// can't be told.
    fn shell_in_background(&self) -> bool {
        let Some(pid) = self.pid else {
            return false;
        };
        let out = std::process::Command::new("/bin/ps")
            .args(["-o", "tpgid=", "-p", &pid.to_string()])
            .output();
        let foreground = out
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|t| t.trim().parse::<i64>().ok());
        foreground.is_some_and(|g| g > 0 && g != i64::from(pid))
    }

    /// Forgets that its program asked to be told of colour scheme changes:
    /// the program that asked has ended, and its shell must never be sent one.
    pub fn forget_color_scheme_events(&self) {
        self.screen.core.lock().term.forget_color_scheme_events();
    }

    /// Types `input` whoever is active (the hub's own writes: prompts from
    /// agents, a holder's client).
    pub fn write(&self, input: &[u8]) -> Result<()> {
        self.io.write(input)
    }

    /// The person's words were put in its prompt for them (not sent): it
    /// holds what Desk would type in, as their own typing does.
    pub fn person_typed(&self) {
        self.typed.store(
            chrono::Utc::now().timestamp_millis(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// When the person last typed in it, if they have since it started.
    pub fn typed_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        match self.typed.load(std::sync::atomic::Ordering::Relaxed) {
            0 => None,
            ms => chrono::DateTime::from_timestamp_millis(ms),
        }
    }

    /// Resizes the terminal whoever is active.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let (cols, rows) = (cols.max(2), rows.max(2));
        self.io.resize(cols, rows)?;
        self.screen.resized(cols, rows);
        Ok(())
    }

    pub fn kill(&self) {
        self.io.kill();
    }

    /// From now on its output is dropped unread: nothing it draws reaches
    /// the screen, its history or a viewer. For a terminal a secret is
    /// about to be typed into, which it may echo.
    pub fn blind(&self) {
        self.screen
            .blind
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the hub's own child was waited for, so its pid is no longer
    /// its own to signal.
    pub fn reaped(&self) -> bool {
        self.reaped_at().is_some()
    }

    /// When the hub's own child was waited for, if it was.
    pub fn reaped_at(&self) -> Option<std::time::Instant> {
        *self.screen.reaped.lock()
    }

    /// The screen as plain text: the history and screen, or the alternate
    /// screen while a program has it.
    pub fn text(&self) -> String {
        self.screen.text()
    }

    /// The process's working directory as the system knows it, else the
    /// one the shell last reported (OSC 7, which an `ssh` session would set
    /// to a remote path, so it comes second).
    pub fn cwd(&self) -> Option<String> {
        self.pid
            .and_then(crate::cwd::of_pid)
            .or_else(|| self.screen.cwd())
    }
}

impl Viewer {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn is_active(&self) -> bool {
        self.pty.screen.core.lock().active == Some(self.id)
    }

    /// Makes this viewer the active one, at the size it last asked for.
    pub fn claim(&self) {
        let size = self
            .pty
            .screen
            .core
            .lock()
            .activate(self.id, &self.pty.screen.tx);
        if let Some((cols, rows)) = size {
            let _ = self.pty.resize(cols, rows);
        }
    }

    /// Keystrokes (and the viewer's answers to queries); dropped unless
    /// this viewer is active. Whether they were sent.
    pub fn input(&self, bytes: &[u8]) -> Result<bool> {
        if !self.is_active() {
            return Ok(false);
        }
        if typed(bytes) {
            self.pty.typed.store(
                chrono::Utc::now().timestamp_millis(),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        self.pty.write(bytes).map(|()| true)
    }

    /// The viewer's size: applied when it is active, remembered for when
    /// it becomes so.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<bool> {
        let active = {
            let mut core = self.pty.screen.core.lock();
            if let Some(v) = core.viewers.iter_mut().find(|(v, _)| *v == self.id) {
                v.1 = Some((cols.max(2), rows.max(2)));
            }
            core.active == Some(self.id)
        };
        if !active {
            return Ok(false);
        }
        self.pty.resize(cols, rows).map(|()| true)
    }

    /// The screen without history and a fresh receiver, for a viewer whose
    /// receiver fell behind.
    pub fn resync(&self) -> (Vec<Frame>, broadcast::Receiver<Frame>) {
        let core = self.pty.screen.core.lock();
        let rx = self.pty.screen.tx.subscribe();
        let (cols, rows) = core.term.size();
        let frames = vec![
            Frame::Size { cols, rows },
            Frame::Active(core.active),
            Frame::Bytes(Bytes::from(core.term.snapshot(false))),
        ];
        (frames, rx)
    }
}

/// When the active viewer goes, the most recently attached one left takes
/// over (a window's terminal reattaching before its old socket closed must
/// not end up unable to type), at its size; with none left, no one is.
impl Drop for Viewer {
    fn drop(&mut self) {
        let size = {
            let mut core = self.pty.screen.core.lock();
            core.viewers.retain(|(v, _)| *v != self.id);
            if core.active != Some(self.id) {
                return;
            }
            match core.viewers.last().map(|(v, _)| *v) {
                Some(next) => core.activate(next, &self.pty.screen.tx),
                None => {
                    core.active = None;
                    let _ = self.pty.screen.tx.send(Frame::Active(None));
                    None
                }
            }
        };
        if let Some((cols, rows)) = size {
            let _ = self.pty.resize(cols, rows);
        }
    }
}

/// The hub's own child in a PTY; killed when dropped.
pub(crate) struct Local {
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// To the thread that writes to the terminal, so a terminal that isn't
    /// reading never blocks whoever types into it.
    input: std::sync::mpsc::Sender<Vec<u8>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pub(crate) pid: Option<u32>,
}

impl Local {
    pub(crate) fn spawn(
        spec: &Spawn,
        cols: u16,
        rows: u16,
        history: usize,
        palette: Option<crate::vt::Palette>,
        unseen: bool,
    ) -> Result<(Self, Arc<Screen>, oneshot::Receiver<Exit>)> {
        let (cols, rows) = (cols.max(2), rows.max(2));
        let pair = native_pty_system()
            .openpty(size(cols, rows))
            .map_err(failed("opening a pseudo-terminal"))?;
        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        cmd.cwd(&spec.cwd);
        cmd.env_clear();
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(failed(&format!("starting {}", spec.program.display())))?;
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(failed("reading the terminal"))?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(failed("writing the terminal"))?;
        let (input, queue) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name("desk-pty-write".into())
            .spawn(move || {
                while let Ok(bytes) = queue.recv() {
                    if writer
                        .write_all(&bytes)
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        return;
                    }
                }
            })
            .context("starting the terminal writer")?;
        let screen = Screen::with(cols, rows, history, palette, unseen);

        let (sink, answer) = (Arc::clone(&screen), input.clone());
        // Dropped when the reader has taken the last of the output, so the
        // exit watcher can tell the output is all in the screen.
        let (drained, drained_rx) = std::sync::mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("desk-pty-read".into())
            .spawn(move || {
                let _drained = drained;
                let mut buf = [0u8; 8192];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if let Some(replies) = sink.push(&buf[..n]) {
                        let _ = answer.send(replies);
                    }
                }
            })
            .context("starting the terminal reader")?;

        let (exit_tx, exit_rx) = oneshot::channel();
        let ended = Arc::clone(&screen);
        std::thread::Builder::new()
            .name("desk-pty-wait".into())
            .spawn(move || {
                let code = child
                    .wait()
                    .ok()
                    .map(|s: portable_pty::ExitStatus| s.exit_code());
                *ended.reaped.lock() = Some(std::time::Instant::now());
                // The child can be reaped before the reader has taken its
                // last output; announcing the end first would cut that
                // output off for anyone who stops listening at the end. A
                // descendant holding the terminal open must not hold this
                // up for long, so the wait is bounded.
                let _ = drained_rx.recv_timeout(std::time::Duration::from_secs(2));
                ended.exited();
                let _ = exit_tx.send(code);
            })
            .context("starting the exit watcher")?;

        Ok((
            Self {
                master: Mutex::new(pair.master),
                input,
                killer: Mutex::new(killer),
                pid,
            },
            screen,
            exit_rx,
        ))
    }
}

impl Io for Local {
    fn write(&self, input: &[u8]) -> Result<()> {
        self.input
            .send(input.to_vec())
            .map_err(|_| eyre::eyre!("the terminal is closed"))
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .lock()
            .resize(size(cols, rows))
            .map_err(failed("resizing the terminal"))
    }

    fn kill(&self) {
        let _ = self.killer.lock().kill();
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Terminal output with escape sequences removed and blank runs collapsed,
/// for reading rather than rendering.
pub fn plain_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters, then one final byte in @..~
                Some('[') => {
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ST
                Some(']') => {
                    while let Some(n) = chars.next() {
                        if n == '\x07' || (n == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            c if c == '\n' || c == '\t' || !c.is_control() => out.push(c),
            _ => {}
        }
    }
    let mut kept: Vec<&str> = Vec::new();
    for line in out.lines().map(str::trim_end) {
        if !(line.is_empty() && kept.last().is_some_and(|l| l.is_empty())) {
            kept.push(line);
        }
    }
    kept.join("\n").trim().to_string()
}

/// portable-pty reports `anyhow` errors; carry their text into eyre.
fn failed<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> eyre::Report + '_ {
    move |e| eyre::eyre!("{what}: {e}")
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;

    pub(crate) fn sh(script: &str) -> Spawn {
        Spawn {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: std::env::temp_dir(),
            env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
        }
    }

    fn spawn(script: &str) -> (Arc<Pty>, oneshot::Receiver<Exit>) {
        let (pty, exit) = Pty::spawn(&sh(script), 80, 24, 100).unwrap();
        (Arc::new(pty), exit)
    }

    pub(crate) async fn wait_for(pty: &Pty, needle: &str) -> String {
        for _ in 0..100 {
            let text = pty.text();
            if text.contains(needle) {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("terminal never showed {needle:?}");
    }

    /// Output frames until `needle` shows up in them.
    async fn read_until(rx: &mut broadcast::Receiver<Frame>, needle: &str) -> String {
        let mut seen = String::new();
        while !seen.contains(needle) {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("output in time")
            {
                Ok(Frame::Bytes(b)) => seen.push_str(&String::from_utf8_lossy(&b)),
                Ok(_) => {}
                Err(e) => panic!("{e}"),
            }
        }
        seen
    }

    fn snapshot_of(frames: &[Frame]) -> String {
        frames
            .iter()
            .find_map(|f| match f {
                Frame::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .unwrap()
    }

    #[tokio::test]
    async fn a_program_that_asked_is_told_the_terminal_turned_light() {
        let (quiet, _q) = spawn("stty raw -echo; printf 'ready'; sleep 5");
        wait_for(&quiet, "ready").await;
        assert!(
            !quiet.tell_ground(crate::vt::Ground::Light, false),
            "a program that didn't ask is sent nothing: its shell would echo it"
        );
        let (pty, _exit) = spawn(
            "stty raw -echo; printf '\\033[?2031hready\\r\\n'; head -c 9 | od -An -c | tr -d ' '; sleep 5",
        );
        wait_for(&pty, "ready").await;
        assert!(pty.tell_ground(crate::vt::Ground::Light, false));
        wait_for(&pty, "033[?997;2n").await;
    }

    #[tokio::test]
    async fn a_shell_at_its_prompt_is_never_told_though_its_program_asked() {
        // The program asked, then left the shell in the foreground (it
        // crashed): the notice would be typed at the prompt.
        let (shell, _s) = spawn(
            "stty raw -echo; printf '\\033[?2031hready\\r\\n'; read x; printf 'typed:%s\\r\\n' \"$x\"; sleep 5",
        );
        wait_for(&shell, "ready").await;
        assert!(!shell.tell_ground(crate::vt::Ground::Light, true));
        // A program in the foreground of it (job control on) is told.
        let (pty, _exit) = spawn(
            "set -m; stty raw -echo; printf '\\033[?2031hready\\r\\n'; head -c 9 | od -An -c | tr -d ' '; sleep 5",
        );
        wait_for(&pty, "ready").await;
        let mut told = false;
        for _ in 0..50 {
            // Until the pipeline is the foreground job.
            if pty.tell_ground(crate::vt::Ground::Light, true) {
                told = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(told);
        wait_for(&pty, "033[?997;2n").await;
    }

    #[tokio::test]
    async fn output_lands_on_the_screen_and_exit_code_is_reported() {
        let (pty, exit) = spawn("printf 'hello desk\\n'; exit 3");
        wait_for(&pty, "hello desk").await;
        assert_eq!(exit.await.unwrap(), Some(3));
    }

    #[tokio::test]
    async fn input_reaches_the_process_and_output_streams_to_attached_viewers() {
        let (pty, _exit) = spawn("read line; echo \"got:$line\"");
        let (viewer, _, mut rx) = pty.attach();
        assert!(viewer.input(b"ping\r").unwrap());
        read_until(&mut rx, "got:ping").await;
    }

    #[tokio::test]
    async fn attaching_mid_session_restores_the_screen_and_its_modes() {
        let (pty, _exit) = spawn(
            "printf 'before\\n\\033[?2004h\\033[?1004h\\033[?1049h\\033[2J\\033[Hin the app'; sleep 30",
        );
        wait_for(&pty, "in the app").await;
        let (_viewer, frames, _) = pty.attach();
        assert!(matches!(frames[0], Frame::Size { cols: 80, rows: 24 }));
        let snap = snapshot_of(&frames);
        for mode in [
            "\x1b[?2004h",
            "\x1b[?1004h",
            "\x1b[?1049h",
            "in the app",
            "before",
        ] {
            assert!(snap.contains(mode), "snapshot lacks {mode:?}");
        }
        pty.kill();
    }

    #[tokio::test]
    async fn only_the_active_viewers_keys_and_size_count() {
        let (pty, _exit) = spawn("stty -echo; while read line; do echo \"got:$line\"; done");
        let (first, frames, mut rx) = pty.attach();
        assert!(matches!(frames[1], Frame::Active(Some(id)) if id == first.id()));
        let (mirror, frames, _) = pty.attach();
        assert!(matches!(frames[1], Frame::Active(Some(id)) if id == first.id()));
        assert!(!mirror.input(b"from-mirror\r").unwrap());
        assert!(!mirror.resize(50, 10).unwrap());
        assert!(first.input(b"from-first\r").unwrap());
        let seen = read_until(&mut rx, "got:from-first").await;
        assert!(!seen.contains("from-mirror"));

        assert!(first.resize(100, 30).unwrap());
        assert_eq!(pty.screen().snapshot(false).0, 100);
        mirror.claim();
        assert!(!first.is_active());
        assert!(mirror.input(b"now-mirror\r").unwrap());
        assert!(!first.input(b"stale\r").unwrap());
        let seen = read_until(&mut rx, "got:now-mirror").await;
        assert!(!seen.contains("stale"));
        // Sizes go out to every viewer, in order with the output.
        mirror.resize(90, 20).unwrap();
        let size = loop {
            if let Frame::Size { cols, rows } = rx.recv().await.unwrap() {
                break (cols, rows);
            }
        };
        assert_eq!(size, (90, 20));
        // The first asked for 70x15 while mirroring; it gets that when the
        // active one goes and it takes over.
        assert!(!first.resize(70, 15).unwrap());
        drop(mirror);
        assert!(first.is_active());
        assert_eq!(pty.screen().snapshot(false).0, 70);
        assert!(first.input(b"first-again\r").unwrap());
        read_until(&mut rx, "got:first-again").await;
        drop(first);
        let (late, frames, _) = pty.attach();
        assert!(matches!(frames[1], Frame::Active(Some(id)) if id == late.id()));
        pty.kill();
    }

    #[tokio::test]
    async fn queries_are_answered_by_the_hub_only_while_no_viewer_is_active() {
        // Asks DA1 and prints what came back.
        let script =
            "stty raw -echo; printf '\\033[c'; dd bs=1 count=7 2>/dev/null | od -An -c; sleep 30";
        let (pty, _exit) = spawn(script);
        wait_for(&pty, "?   1   ;   2   c").await;
        pty.kill();

        let (pty, _exit) = spawn(&format!("sleep 1; {script}"));
        let (viewer, _, mut rx) = pty.attach();
        read_until(&mut rx, "\x1b[c").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !pty.text().contains("?   1"),
            "the active viewer answers, not the hub"
        );
        viewer.input(b"\x1b[?6;4c").unwrap();
        wait_for(&pty, "6   ;   4   c").await;
        pty.kill();
    }

    /// Goes nowhere.
    struct Null;

    impl Io for Null {
        fn write(&self, _: &[u8]) -> Result<()> {
            Ok(())
        }
        fn resize(&self, _: u16, _: u16) -> Result<()> {
            Ok(())
        }
        fn kill(&self) {}
    }

    #[tokio::test]
    async fn a_viewer_that_falls_behind_is_resynced_with_the_screen() {
        let screen = Screen::new(40, 10, 100, None);
        let pty = Arc::new(Pty::from_parts(Arc::clone(&screen), Box::new(Null), None));
        screen.push(b"row 0\r\n");
        let (viewer, _, mut rx) = pty.attach();
        for i in 1..3000 {
            screen.push(format!("row {i}\r\n").as_bytes());
        }
        // Half an escape sequence is where the viewer's bytes stop.
        screen.push(b"done\x1b[3");
        screen.push(b"1mred");
        let lagged = loop {
            match rx.recv().await {
                Err(broadcast::error::RecvError::Lagged(n)) => break n,
                Ok(_) => {}
                Err(e) => panic!("{e}"),
            }
        };
        assert!(lagged > 0);
        let (frames, _rx) = viewer.resync();
        let snap = snapshot_of(&frames);
        assert!(snap.starts_with('\x18'));
        assert!(snap.contains("row 2999") && snap.contains("done\x1b[31mred"));
        assert!(!snap.contains("row 0\r"), "a resync doesn't resend history");
    }

    #[test]
    fn a_blind_screen_takes_nothing_in_but_still_answers_what_its_program_waits_on() {
        let screen = Screen::new(80, 24, 100, None);
        screen
            .blind
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let replies = screen.push(
            b"Token: secret-123\x1b[6n more \x1b[c\x1b[0c\x1b[5n\x1b]10;?\x07\x1b]11;?\x1b\\\x1b[?u\x1b[>1u\x1b]12;?\x07",
        );
        assert_eq!(
            replies.as_deref(),
            Some(
                &b"\x1b[1;1R\x1b[?1;2c\x1b[?1;2c\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[?0u"[..]
            )
        );
        // Devin's own, as 3000.12.57 asks them before its code prompt.
        let devin = include_bytes!("../fixtures/devin/3-manual.bin");
        let replies = screen.push(devin).unwrap();
        assert_eq!(
            replies,
            [BLIND_FOREGROUND, BLIND_BACKGROUND].concat().repeat(2),
            "{}",
            String::from_utf8_lossy(&replies)
        );
        // A query cut in two is answered once it is whole, and only then.
        assert_eq!(screen.push(b"code: abc\x1b]1"), None);
        assert_eq!(screen.push(b"1;?").as_deref(), None);
        assert_eq!(screen.push(b"\x07").as_deref(), Some(BLIND_BACKGROUND));
        assert_eq!(screen.push(b"x\x1b["), None);
        assert_eq!(screen.push(b"?u").as_deref(), Some(&b"\x1b[?0u"[..]));
        // What is kept between chunks is only ever a query's start.
        assert_eq!(screen.push(b"secret\x1b"), None);
        assert_eq!(screen.blind_tail.lock().as_slice(), b"\x1b");
        assert_eq!(screen.push(b"secret-789"), None);
        assert!(screen.blind_tail.lock().is_empty());
        assert!(!screen.text().contains("secret"), "{}", screen.text());
        assert_eq!(screen.push(b"secret-456"), None);
        assert!(screen.text().trim().is_empty());
        // A viewer never sees blind output, so the hub answers even then.
        screen.core.lock().active = Some(1);
        assert_eq!(screen.push(b"\x1b[6n").as_deref(), Some(&b"\x1b[1;1R"[..]));
    }

    #[tokio::test]
    async fn an_unseen_terminal_answers_colours_and_kitty_flags_and_reads_its_screen() {
        // Asks what Devin's login asks, and prints what came back.
        let script = "stty raw -echo; printf 'Hi\\033]10;?\\007\\033]11;?\\007\\033[?u\\033[c'; \
             dd bs=1 count=62 2>/dev/null | od -An -c | tr -d ' \\n'; sleep 30";
        let (pty, _exit) = Pty::spawn_unseen(&sh(script), 120, 10, 0).unwrap();
        let said = wait_for(&pty, "?1;2c").await;
        assert!(
            said.contains(
                "033]10;rgb:ffff/ffff/ffff033\\033]11;rgb:0000/0000/0000033\\033[?0u033[?1;2c"
            ),
            "{said}"
        );
        assert!(said.starts_with("Hi"), "{said}");
        pty.kill();
        // A terminal a person is shown leaves the kitty query to them.
        let seen = Screen::new(80, 24, 0, None);
        assert_eq!(seen.push(b"\x1b[?u\x1b]10;?\x07"), None);
    }

    #[test]
    fn plain_text_drops_escapes_and_blank_runs() {
        let raw = b"\x1b[2J\x1b[38;5;208m\xe2\x97\x8f Bash\x1b[0m(cargo test)\r\n\r\n\r\n\x1b]0;title\x07done  \r\n";
        assert_eq!(plain_text(raw), "\u{25cf} Bash(cargo test)\n\ndone");
    }

    #[tokio::test]
    async fn kill_ends_the_process_and_tells_viewers() {
        let (pty, exit) = spawn("sleep 30");
        let (_viewer, _, mut rx) = pty.attach();
        pty.kill();
        tokio::time::timeout(Duration::from_secs(5), exit)
            .await
            .expect("exit after kill")
            .unwrap();
        loop {
            if let Frame::Exit = rx.recv().await.unwrap() {
                break;
            }
        }
    }

    /// Real Claude Code with no viewer attached: the hub's answers get it
    /// past its terminal probes. Run with DESK_REAL_CLAUDE=<path to claude>.
    #[tokio::test]
    #[ignore = "needs a real Claude Code: DESK_REAL_CLAUDE=<path>"]
    async fn real_claude_code_starts_with_no_viewer_attached() {
        let Ok(claude) = std::env::var("DESK_REAL_CLAUDE") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let env: BTreeMap<String, String> = std::env::vars()
            .filter(|(k, _)| !k.starts_with("CLAUDE"))
            .chain([("TERM".to_string(), "xterm-256color".to_string())])
            .collect();
        let spec = Spawn {
            program: claude.into(),
            args: vec![],
            cwd: dir.path().into(),
            env,
        };
        let started = std::time::Instant::now();
        let (pty, _exit) = Pty::spawn(&spec, 120, 40, 100).unwrap();
        let mut shown = String::new();
        for _ in 0..300 {
            shown = pty.text();
            if shown.to_lowercase().contains("trust") || shown.contains('>') {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        eprintln!("first screen after {:?}:\n{shown}", started.elapsed());
        pty.kill();
        assert!(!shown.trim().is_empty(), "Claude Code drew nothing");
    }
}

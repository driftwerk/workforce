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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
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
    active: Option<u64>,
    next_viewer: u64,
    exited: bool,
}

/// The screen model and the viewers' channel, shared with whatever reads
/// the process's output.
pub(crate) struct Screen {
    core: Mutex<Core>,
    tx: broadcast::Sender<Frame>,
}

impl Screen {
    pub(crate) fn new(cols: u16, rows: u16, history: usize) -> Arc<Self> {
        Arc::new(Self {
            core: Mutex::new(Core {
                term: Term::new(cols, rows, history),
                active: None,
                next_viewer: 1,
                exited: false,
            }),
            tx: broadcast::channel(1024).0,
        })
    }

    /// Takes in output. Returns the replies the hub owes the program's
    /// queries when no viewer is active to answer them.
    pub(crate) fn push(&self, chunk: &[u8]) -> Option<Vec<u8>> {
        let mut core = self.core.lock();
        let replies = core.term.feed(chunk);
        let _ = self.tx.send(Frame::Bytes(Bytes::copy_from_slice(chunk)));
        (core.active.is_none() && !replies.is_empty()).then_some(replies)
    }

    /// Starts over from a snapshot of the screen (a holder's, after the
    /// hub reconnected or fell behind it); viewers are resynced.
    pub(crate) fn reset(&self, cols: u16, rows: u16, history: usize, snapshot: &[u8]) {
        let mut core = self.core.lock();
        let mut term = Term::new(cols, rows, history);
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

/// One terminal.
pub struct Pty {
    screen: Arc<Screen>,
    io: Box<dyn Io>,
    pid: Option<u32>,
}

/// One viewer of a [`Pty`]; dropping it detaches it, giving up the active
/// role if it held it.
pub struct Viewer {
    id: u64,
    pty: Arc<Pty>,
}

impl Pty {
    pub(crate) fn from_parts(screen: Arc<Screen>, io: Box<dyn Io>, pid: Option<u32>) -> Self {
        Self { screen, io, pid }
    }

    /// Starts `spec` as the hub's own child: it ends when this does.
    pub fn spawn(
        spec: &Spawn,
        cols: u16,
        rows: u16,
        history: usize,
    ) -> Result<(Self, oneshot::Receiver<Exit>)> {
        let (local, screen, exit) = Local::spawn(spec, cols, rows, history)?;
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

    /// Types `input` whoever is active (the hub's own writes: prompts from
    /// agents, a holder's client).
    pub fn write(&self, input: &[u8]) -> Result<()> {
        self.io.write(input)
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

    /// The screen as plain text: the history and screen, or the alternate
    /// screen while a program has it.
    pub fn text(&self) -> String {
        self.screen.text()
    }

    /// The working directory the shell last reported (OSC 7), else the
    /// process's own as the system knows it.
    pub fn cwd(&self) -> Option<String> {
        self.screen
            .cwd()
            .or_else(|| self.pid.and_then(crate::cwd::of_pid))
    }
}

impl Viewer {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn is_active(&self) -> bool {
        self.pty.screen.core.lock().active == Some(self.id)
    }

    /// Makes this viewer the active one.
    pub fn claim(&self) {
        let mut core = self.pty.screen.core.lock();
        if core.active != Some(self.id) {
            core.active = Some(self.id);
            let _ = self.pty.screen.tx.send(Frame::Active(Some(self.id)));
        }
    }

    /// Keystrokes (and the viewer's answers to queries); dropped unless
    /// this viewer is active. Whether they were sent.
    pub fn input(&self, bytes: &[u8]) -> Result<bool> {
        if !self.is_active() {
            return Ok(false);
        }
        self.pty.write(bytes).map(|()| true)
    }

    /// The viewer's size; applies only when it is active.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<bool> {
        if !self.is_active() {
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

impl Drop for Viewer {
    fn drop(&mut self) {
        let mut core = self.pty.screen.core.lock();
        if core.active == Some(self.id) {
            core.active = None;
            let _ = self.pty.screen.tx.send(Frame::Active(None));
        }
    }
}

/// The hub's own child in a PTY; killed when dropped.
pub(crate) struct Local {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pub(crate) pid: Option<u32>,
}

impl Local {
    pub(crate) fn spawn(
        spec: &Spawn,
        cols: u16,
        rows: u16,
        history: usize,
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
        let writer = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .map_err(failed("writing the terminal"))?,
        ));
        let screen = Screen::new(cols, rows, history);

        let (sink, answer) = (Arc::clone(&screen), Arc::clone(&writer));
        std::thread::Builder::new()
            .name("desk-pty-read".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if let Some(replies) = sink.push(&buf[..n]) {
                        let mut w = answer.lock();
                        let _ = w.write_all(&replies).and_then(|()| w.flush());
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
                ended.exited();
                let _ = exit_tx.send(code);
            })
            .context("starting the exit watcher")?;

        Ok((
            Self {
                master: Mutex::new(pair.master),
                writer,
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
        let mut w = self.writer.lock();
        w.write_all(input)?;
        w.flush()?;
        Ok(())
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
        drop(mirror);
        assert!(!first.is_active());
        assert!(!first.input(b"nobody\r").unwrap());
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
        let screen = Screen::new(40, 10, 100);
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
}

//! A CLI running in a pseudo-terminal the hub owns. Output goes into a
//! bounded scrollback and out to every attached viewer; attaching returns
//! the scrollback and a receiver taken under the same lock the reader
//! appends under, so a viewer sees every byte exactly once.

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use eyre::{Context, Result};
use parking_lot::Mutex;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::{broadcast, oneshot};

pub const DEFAULT_COLS: u16 = 120;
pub const DEFAULT_ROWS: u16 = 40;

#[derive(Debug, Clone, PartialEq)]
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

/// How the process ended: its exit code, or `None` when it could not be read.
pub type Exit = Option<u32>;

struct Scrollback {
    bytes: VecDeque<u8>,
    cap: usize,
    truncated: bool,
}

impl Scrollback {
    fn new(cap: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            cap,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        let excess = self.bytes.len().saturating_sub(self.cap);
        if excess > 0 {
            self.bytes.drain(..excess);
            self.truncated = true;
        }
    }

    fn snapshot(&self) -> Bytes {
        let mut out: Vec<u8> = self.bytes.iter().copied().collect();
        if self.truncated {
            // Cut mid-stream: start at a line boundary, not inside an escape
            // sequence.
            if let Some(nl) = out.iter().position(|&c| c == b'\n') {
                out.drain(..=nl);
            }
        }
        Bytes::from(out)
    }
}

struct Output {
    scrollback: Mutex<Scrollback>,
    tx: broadcast::Sender<Bytes>,
}

impl Output {
    fn push(&self, chunk: &[u8]) {
        let mut sb = self.scrollback.lock();
        sb.push(chunk);
        let _ = self.tx.send(Bytes::copy_from_slice(chunk));
    }
}

pub struct Pty {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    output: Arc<Output>,
    pid: Option<u32>,
}

impl Pty {
    pub fn spawn(spec: &Spawn, scrollback_bytes: usize) -> Result<(Self, oneshot::Receiver<Exit>)> {
        let pair = native_pty_system()
            .openpty(size(DEFAULT_COLS, DEFAULT_ROWS))
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
        let writer = pair
            .master
            .take_writer()
            .map_err(failed("writing the terminal"))?;
        let output = Arc::new(Output {
            scrollback: Mutex::new(Scrollback::new(scrollback_bytes)),
            tx: broadcast::channel(1024).0,
        });

        let sink = Arc::clone(&output);
        std::thread::Builder::new()
            .name("desk-pty-read".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    sink.push(&buf[..n]);
                }
            })
            .context("starting the terminal reader")?;

        let (exit_tx, exit_rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("desk-pty-wait".into())
            .spawn(move || {
                let code = child
                    .wait()
                    .ok()
                    .map(|s: portable_pty::ExitStatus| s.exit_code());
                let _ = exit_tx.send(code);
            })
            .context("starting the exit watcher")?;

        Ok((
            Self {
                master: Mutex::new(pair.master),
                writer: Mutex::new(writer),
                killer: Mutex::new(killer),
                output,
                pid,
            },
            exit_rx,
        ))
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn attach(&self) -> (Bytes, broadcast::Receiver<Bytes>) {
        let sb = self.output.scrollback.lock();
        (sb.snapshot(), self.output.tx.subscribe())
    }

    pub fn write(&self, input: &[u8]) -> Result<()> {
        let mut w = self.writer.lock();
        w.write_all(input)?;
        w.flush()?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .lock()
            .resize(size(cols.max(2), rows.max(2)))
            .map_err(failed("resizing the terminal"))
    }

    pub fn kill(&self) {
        let _ = self.killer.lock().kill();
    }
}

impl Drop for Pty {
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
mod tests {
    use super::*;
    use std::time::Duration;

    fn sh(script: &str) -> Spawn {
        Spawn {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: std::env::temp_dir(),
            env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
        }
    }

    async fn wait_for(pty: &Pty, needle: &str) -> String {
        for _ in 0..100 {
            let (snap, _) = pty.attach();
            let text = String::from_utf8_lossy(&snap).into_owned();
            if text.contains(needle) {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("terminal never showed {needle:?}");
    }

    #[tokio::test]
    async fn output_lands_in_scrollback_and_exit_code_is_reported() {
        let (pty, exit) = Pty::spawn(&sh("printf 'hello desk\\n'; exit 3"), 4096).unwrap();
        wait_for(&pty, "hello desk").await;
        assert_eq!(exit.await.unwrap(), Some(3));
    }

    #[tokio::test]
    async fn input_reaches_the_process_and_output_streams_to_attached_viewers() {
        let (pty, _exit) = Pty::spawn(&sh("read line; echo \"got:$line\""), 4096).unwrap();
        let (_, mut rx) = pty.attach();
        pty.write(b"ping\r").unwrap();
        let mut seen = String::new();
        while !seen.contains("got:ping") {
            let chunk = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("output in time")
                .unwrap();
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    #[test]
    fn scrollback_keeps_the_newest_bytes_from_a_line_start() {
        let mut sb = Scrollback::new(10);
        sb.push(b"aaaa\nbbbb\ncccc");
        assert_eq!(&sb.snapshot()[..], b"bbbb\ncccc");
        sb.push(b"dd");
        assert_eq!(&sb.snapshot()[..], b"ccccdd");
        let mut small = Scrollback::new(64);
        small.push(b"one\ntwo");
        assert_eq!(&small.snapshot()[..], b"one\ntwo");
    }

    #[test]
    fn plain_text_drops_escapes_and_blank_runs() {
        let raw = b"\x1b[2J\x1b[38;5;208m\xe2\x97\x8f Bash\x1b[0m(cargo test)\r\n\r\n\r\n\x1b]0;title\x07done  \r\n";
        assert_eq!(plain_text(raw), "\u{25cf} Bash(cargo test)\n\ndone");
    }

    #[tokio::test]
    async fn kill_ends_the_process() {
        let (pty, exit) = Pty::spawn(&sh("sleep 30"), 1024).unwrap();
        pty.kill();
        tokio::time::timeout(Duration::from_secs(5), exit)
            .await
            .expect("exit after kill")
            .unwrap();
    }
}

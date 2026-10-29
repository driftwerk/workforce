//! The PTY holder: a small long-lived process (`workforce desk-pty-holder`)
//! that owns Desk's terminals so they outlive the hub. The hub starts every
//! terminal through it and, after a restart, takes back the ones still
//! running, screens and all.
//!
//! Protocol, over a unix socket only its owner can open: frames of
//! `[u32 length][u32 header length][header JSON][payload]`, big-endian. The
//! header is a [`Msg`]; output, input and snapshots carry their bytes as the
//! payload. The first frame is `hello {version, token}`; the holder answers
//! `welcome` or `denied` and closes.
//!
//! The holder keeps each terminal's screen (as the hub does, [`crate::vt`]),
//! so an attaching hub is sent a snapshot and then the output. While no hub
//! is attached to a terminal the holder answers its queries; a terminal
//! that ends with no hub attached is kept, exit code and all, until a hub
//! has seen it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use eyre::{Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::pty::{Exit, Frame, Io, Pty, Screen, Spawn, Viewer};

pub const PROTOCOL_VERSION: u32 = 1;
/// The environment variable the holder reads its token from.
pub const TOKEN_ENV: &str = "WORKFORCE_PTY_HOLDER_TOKEN";
const MAX_FRAME: usize = 16 << 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub(crate) enum Msg {
    Hello {
        version: u32,
        token: String,
    },
    Welcome {
        version: u32,
    },
    Denied {
        error: String,
    },
    Spawn {
        req: u64,
        id: Uuid,
        spec: Spawn,
        cols: u16,
        rows: u16,
        history: usize,
        meta: Value,
    },
    Spawned {
        req: u64,
        id: Uuid,
        pid: Option<u32>,
    },
    Failed {
        req: u64,
        error: String,
    },
    List {
        req: u64,
    },
    Listing {
        req: u64,
        ptys: Vec<Held>,
    },
    /// Subscribe: a `snapshot`, then `output` until `exited`.
    Attach {
        id: Uuid,
    },
    /// Payload: the screen, with its history.
    Snapshot {
        id: Uuid,
        cols: u16,
        rows: u16,
    },
    /// Payload: what the terminal wrote.
    Output {
        id: Uuid,
    },
    /// Payload: what to type.
    Input {
        id: Uuid,
    },
    Resize {
        id: Uuid,
        cols: u16,
        rows: u16,
    },
    Kill {
        id: Uuid,
    },
    Exited {
        id: Uuid,
        code: Option<u32>,
    },
    /// No such terminal.
    Gone {
        id: Uuid,
    },
    /// Forget an ended terminal.
    Reap {
        id: Uuid,
    },
}

/// A terminal the holder has, as `list` reports it. `meta` is whatever the
/// hub gave at spawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Held {
    pub id: Uuid,
    pub pid: Option<u32>,
    pub meta: Value,
    pub cols: u16,
    pub rows: u16,
    pub exited: bool,
    pub code: Option<u32>,
}

pub(crate) fn encode(msg: &Msg, payload: &[u8]) -> Vec<u8> {
    let header = serde_json::to_vec(msg).expect("a message always serialises");
    let total = 4 + header.len() + payload.len();
    let mut out = Vec::with_capacity(4 + total);
    out.extend_from_slice(&u32::try_from(total).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(
        &u32::try_from(header.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    out.extend_from_slice(&header);
    out.extend_from_slice(payload);
    out
}

/// The next frame, or `None` at a clean end of stream.
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<(Msg, Vec<u8>)>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let total = u32::from_be_bytes(len) as usize;
    if !(4..=MAX_FRAME).contains(&total) {
        eyre::bail!("a frame of {total} bytes");
    }
    let mut body = vec![0u8; total];
    r.read_exact(&mut body).await?;
    let header_len = u32::from_be_bytes(body[..4].try_into().expect("4 bytes")) as usize;
    if header_len > total - 4 {
        eyre::bail!("a header longer than its frame");
    }
    let msg = serde_json::from_slice(&body[4..4 + header_len]).context("reading a frame header")?;
    Ok(Some((msg, body.split_off(4 + header_len))))
}

/// Whether the two tokens are equal, in time that doesn't depend on where
/// they differ.
pub(crate) fn same_token(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether the peer runs as this process's user; `None` when it hung up
/// before it could be asked (another holder checking the socket is in use).
fn same_user(stream: &UnixStream) -> Option<bool> {
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    stream.peer_cred().ok().map(|c| c.uid() == me)
}

// ---------------------------------------------------------------- holder

struct Entry {
    pty: Arc<Pty>,
    meta: Value,
    exit: watch::Receiver<Option<Exit>>,
}

struct HolderState {
    token: String,
    ptys: Mutex<HashMap<Uuid, Entry>>,
}

/// Prepares `socket`'s folder (owner-only) and clears a stale socket.
/// `Ok(false)` when a holder already answers there.
async fn claim_socket(socket: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    if UnixStream::connect(socket).await.is_ok() {
        return Ok(false);
    }
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    match std::fs::remove_file(socket) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    Ok(true)
}

/// Serves the holder on `socket` until `cancel`, then ends every terminal
/// it holds. Returns at once, successfully, when another holder already
/// serves there.
pub async fn serve(socket: &Path, token: &str, cancel: CancellationToken) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if token.len() < 16 {
        eyre::bail!("the holder's token must be at least 16 characters");
    }
    // macOS keeps a socket's path in 104 bytes, its NUL included.
    if socket.as_os_str().len() > 103 {
        eyre::bail!(
            "{} is too long for a unix socket ({} bytes; at most 103)",
            socket.display(),
            socket.as_os_str().len()
        );
    }
    if !claim_socket(socket).await? {
        tracing::info!(socket = %socket.display(), "a PTY holder is already running");
        return Ok(());
    }
    let listener =
        UnixListener::bind(socket).with_context(|| format!("listening on {}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    // Beside the socket, for whoever must replace this holder (a new
    // protocol version) to find it.
    let pid_file = pid_file(socket);
    std::fs::write(&pid_file, format!("{}\n", std::process::id()))?;
    tracing::info!(socket = %socket.display(), "PTY holder listening");
    let state = Arc::new(HolderState {
        token: token.to_string(),
        ptys: Mutex::new(HashMap::new()),
    });
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => match same_user(&stream) {
                    Some(true) => {
                        tokio::spawn(connection(Arc::clone(&state), stream, cancel.clone()));
                    }
                    Some(false) => tracing::warn!("PTY holder refused a connection from another user"),
                    None => tracing::debug!("a connection closed before it was checked"),
                },
                Err(e) => tracing::warn!(error = %e, "PTY holder accept failed"),
            },
        }
    }
    let ptys: Vec<_> = state.ptys.lock().drain().map(|(_, e)| e.pty).collect();
    tracing::info!(
        count = ptys.len(),
        "PTY holder stopping; ending its terminals"
    );
    ptys.iter().for_each(|p| p.kill());
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(pid_file);
    Ok(())
}

/// Where a holder serving `socket` writes its process id.
pub fn pid_file(socket: &Path) -> PathBuf {
    socket.with_extension("pid")
}

/// `workforce pty-holder`: detaches from whoever started it (its own
/// session, so it outlives the supervisor restarting the hub, and SIGHUP is
/// ignored), reads its token from [`TOKEN_ENV`], and serves until SIGTERM
/// or SIGINT, which end its terminals.
pub async fn run(socket: &Path) -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let token = std::env::var(TOKEN_ENV).map_err(|_| eyre::eyre!("{TOKEN_ENV} is not set"))?;
    // SAFETY: setsid has no preconditions; it fails harmlessly when this
    // process already leads a group.
    if unsafe { libc::setsid() } == -1 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "PTY holder could not start its own session; it ends with its starter's process group"
        );
    }
    let mut hup = signal(SignalKind::hangup())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = hup.recv() => tracing::info!("PTY holder ignores SIGHUP"),
                _ = term.recv() => break,
                _ = int.recv() => break,
            }
        }
        stop.cancel();
    });
    serve(socket, &token, cancel).await
}

async fn connection(state: Arc<HolderState>, stream: UnixStream, cancel: CancellationToken) {
    let (mut rd, mut wr) = stream.into_split();
    let hello = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut rd)).await;
    let denied = match hello {
        Ok(Ok(Some((Msg::Hello { version, token }, _)))) => {
            if !same_token(&token, &state.token) {
                Some("wrong token".to_string())
            } else if version != PROTOCOL_VERSION {
                Some(format!(
                    "this holder speaks protocol {PROTOCOL_VERSION}, not {version}"
                ))
            } else {
                None
            }
        }
        _ => Some("expected hello".to_string()),
    };
    if let Some(error) = denied {
        tracing::warn!(%error, "PTY holder refused a client");
        let _ = wr.write_all(&encode(&Msg::Denied { error }, &[])).await;
        return;
    }
    let (out, mut queue) = mpsc::channel::<Vec<u8>>(256);
    let writer = tokio::spawn(async move {
        while let Some(frame) = queue.recv().await {
            if wr.write_all(&frame).await.is_err() {
                return;
            }
        }
    });
    let _ = out
        .send(encode(
            &Msg::Welcome {
                version: PROTOCOL_VERSION,
            },
            &[],
        ))
        .await;
    // This client's viewers (so, while it is attached, the holder leaves
    // queries to it) and the tasks forwarding their output.
    let mut attached: HashMap<Uuid, (Viewer, tokio::task::JoinHandle<()>)> = HashMap::new();
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => break,
            next = read_frame(&mut rd) => next,
        };
        let frame = match next {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(error = %e, "PTY holder client sent a bad frame");
                break;
            }
        };
        let reply = match frame {
            (
                Msg::Spawn {
                    req,
                    id,
                    spec,
                    cols,
                    rows,
                    history,
                    meta,
                },
                _,
            ) => Some(match spawn(&state, id, &spec, cols, rows, history, meta) {
                Ok(pid) => Msg::Spawned { req, id, pid },
                Err(e) => Msg::Failed {
                    req,
                    error: format!("{e:#}"),
                },
            }),
            (Msg::List { req }, _) => Some(Msg::Listing {
                req,
                ptys: list(&state),
            }),
            (Msg::Attach { id }, _) => {
                let found = state
                    .ptys
                    .lock()
                    .get(&id)
                    .map(|e| (Arc::clone(&e.pty), e.exit.clone()));
                match found {
                    Some((pty, exit)) => {
                        if let Some((_, old)) = attached.remove(&id) {
                            old.abort();
                        }
                        // Claimed, snapshotted and subscribed at once: output
                        // (a program's queries) can't fall between them.
                        let (viewer, first, rx) = pty.attach();
                        let task = tokio::spawn(forward(
                            Arc::clone(&state),
                            id,
                            pty,
                            exit,
                            out.clone(),
                            (first, rx),
                        ));
                        attached.insert(id, (viewer, task));
                        None
                    }
                    None => Some(Msg::Gone { id }),
                }
            }
            (Msg::Input { id }, input) => {
                if let Some(pty) = pty_of(&state, id) {
                    if let Err(e) = pty.write(&input) {
                        tracing::debug!(%id, error = %e, "PTY holder input dropped");
                    }
                }
                None
            }
            (Msg::Resize { id, cols, rows }, _) => {
                if let Some(pty) = pty_of(&state, id) {
                    let _ = pty.resize(cols, rows);
                }
                None
            }
            (Msg::Kill { id }, _) => {
                if let Some(pty) = pty_of(&state, id) {
                    pty.kill();
                }
                None
            }
            (Msg::Reap { id }, _) => {
                let mut ptys = state.ptys.lock();
                if ptys.get(&id).is_some_and(|e| e.exit.borrow().is_some()) {
                    ptys.remove(&id);
                }
                None
            }
            (other, _) => {
                tracing::debug!(?other, "PTY holder ignored a message");
                None
            }
        };
        if let Some(reply) = reply {
            if out.send(encode(&reply, &[])).await.is_err() {
                break;
            }
        }
    }
    for (_, (viewer, task)) in attached {
        task.abort();
        drop(viewer);
    }
    drop(out);
    let _ = writer.await;
}

fn pty_of(state: &HolderState, id: Uuid) -> Option<Arc<Pty>> {
    state.ptys.lock().get(&id).map(|e| Arc::clone(&e.pty))
}

fn spawn(
    state: &Arc<HolderState>,
    id: Uuid,
    spec: &Spawn,
    cols: u16,
    rows: u16,
    history: usize,
    meta: Value,
) -> Result<Option<u32>> {
    if state.ptys.lock().contains_key(&id) {
        eyre::bail!("there is already a terminal {id}");
    }
    let (pty, exit) = Pty::spawn(spec, cols, rows, history)?;
    let pid = pty.pid();
    let (code_tx, code_rx) = watch::channel(None);
    tokio::spawn(async move {
        let code = exit.await.ok().flatten();
        let _ = code_tx.send(Some(code));
    });
    state.ptys.lock().insert(
        id,
        Entry {
            pty: Arc::new(pty),
            meta,
            exit: code_rx,
        },
    );
    tracing::info!(%id, ?pid, program = %spec.program.display(), "PTY holder started a terminal");
    Ok(pid)
}

fn list(state: &HolderState) -> Vec<Held> {
    let ptys = state.ptys.lock();
    let mut all: Vec<Held> = ptys
        .iter()
        .map(|(id, e)| {
            let (cols, rows, _) = e.pty.screen().snapshot(false);
            let exit = *e.exit.borrow();
            Held {
                id: *id,
                pid: e.pty.pid(),
                meta: e.meta.clone(),
                cols,
                rows,
                exited: exit.is_some(),
                code: exit.flatten(),
            }
        })
        .collect();
    all.sort_by_key(|h| h.id);
    all
}

/// Sends one terminal's screen and then its output to a client, a fresh
/// snapshot whenever the client falls behind, and its end — after which
/// the holder forgets it, since a hub has seen it.
async fn forward(
    state: Arc<HolderState>,
    id: Uuid,
    pty: Arc<Pty>,
    mut exit: watch::Receiver<Option<Exit>>,
    out: mpsc::Sender<Vec<u8>>,
    (first, mut rx): (Vec<Frame>, broadcast::Receiver<Frame>),
) {
    let snapshot = |pty: &Pty| {
        let (cols, rows, bytes, rx) = pty.screen().snapshot_and_subscribe(true);
        (encode(&Msg::Snapshot { id, cols, rows }, &bytes), rx)
    };
    let (mut cols, mut rows, mut bytes) = (0, 0, Vec::new());
    for frame in first {
        match frame {
            Frame::Size { cols: c, rows: r } => (cols, rows) = (c, r),
            Frame::Bytes(b) => bytes = b.to_vec(),
            _ => {}
        }
    }
    let snap = encode(&Msg::Snapshot { id, cols, rows }, &bytes);
    if out.send(snap).await.is_err() {
        return;
    }
    let output = |bytes: &[u8]| encode(&Msg::Output { id }, bytes);
    loop {
        tokio::select! {
            frame = rx.recv() => match frame {
                Ok(Frame::Bytes(b)) => {
                    if out.send(output(&b)).await.is_err() { return }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let (again, fresh) = snapshot(&pty);
                    rx = fresh;
                    if out.send(again).await.is_err() { return }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            code = async { exit.wait_for(Option::is_some).await.map(|c| (*c).flatten()) } => {
                let code = code.unwrap_or(None);
                while let Ok(frame) = rx.try_recv() {
                    if let Frame::Bytes(b) = frame {
                        let _ = out.send(output(&b)).await;
                    }
                }
                if out.send(encode(&Msg::Exited { id, code }, &[])).await.is_ok() {
                    state.ptys.lock().remove(&id);
                }
                return;
            }
        }
    }
}

// ---------------------------------------------------------------- client

/// The hub's connection to a holder.
#[derive(Clone)]
pub struct Holder(Arc<Client>);

struct Attached {
    screen: Arc<Screen>,
    history: usize,
    exit: Option<oneshot::Sender<Exit>>,
}

struct Client {
    out: mpsc::UnboundedSender<Vec<u8>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Msg>>>,
    attached: Mutex<HashMap<Uuid, Attached>>,
    next: AtomicU64,
    closed: CancellationToken,
}

impl Client {
    fn send(&self, msg: &Msg, payload: &[u8]) -> Result<()> {
        if self.closed.is_cancelled() {
            eyre::bail!("the PTY holder is gone");
        }
        self.out
            .send(encode(msg, payload))
            .map_err(|_| eyre::eyre!("the PTY holder is gone"))
    }

    fn dispatch(&self, msg: Msg, payload: Vec<u8>) {
        match msg {
            Msg::Spawned { req, .. } | Msg::Failed { req, .. } | Msg::Listing { req, .. } => {
                if let Some(tx) = self.pending.lock().remove(&req) {
                    let _ = tx.send(msg);
                }
            }
            Msg::Snapshot { id, cols, rows } => {
                let found = self
                    .attached
                    .lock()
                    .get(&id)
                    .map(|a| (Arc::clone(&a.screen), a.history));
                if let Some((screen, history)) = found {
                    screen.reset(cols, rows, history, &payload);
                }
            }
            Msg::Output { id } => {
                let screen = self.attached.lock().get(&id).map(|a| Arc::clone(&a.screen));
                if let Some(replies) = screen.and_then(|s| s.push(&payload)) {
                    let _ = self.send(&Msg::Input { id }, &replies);
                }
            }
            Msg::Exited { id, code } => self.ended(id, code),
            Msg::Gone { id } => self.ended(id, None),
            other => tracing::debug!(?other, "unexpected message from the PTY holder"),
        }
    }

    fn ended(&self, id: Uuid, code: Exit) {
        if let Some(mut a) = self.attached.lock().remove(&id) {
            a.screen.exited();
            if let Some(tx) = a.exit.take() {
                let _ = tx.send(code);
            }
        }
    }

    fn lost(&self) {
        self.closed.cancel();
        self.pending.lock().clear();
        let all: Vec<Uuid> = self.attached.lock().keys().copied().collect();
        for id in all {
            self.ended(id, None);
        }
    }
}

/// Keystrokes, sizes and kills for a terminal the holder has.
struct HeldIo {
    client: Arc<Client>,
    id: Uuid,
}

impl Io for HeldIo {
    fn write(&self, input: &[u8]) -> Result<()> {
        self.client.send(&Msg::Input { id: self.id }, input)
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.client.send(
            &Msg::Resize {
                id: self.id,
                cols,
                rows,
            },
            &[],
        )
    }

    fn kill(&self) {
        let _ = self.client.send(&Msg::Kill { id: self.id }, &[]);
    }
}

impl Holder {
    /// Connects and says hello; fails when nothing listens at `socket` or
    /// the holder turns the token or protocol down.
    pub async fn connect(socket: &Path, token: &str) -> Result<Self> {
        let stream = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(socket))
            .await
            .map_err(|_| eyre::eyre!("timed out"))?
            .with_context(|| format!("connecting to {}", socket.display()))?;
        let (mut rd, mut wr) = stream.into_split();
        wr.write_all(&encode(
            &Msg::Hello {
                version: PROTOCOL_VERSION,
                token: token.to_string(),
            },
            &[],
        ))
        .await?;
        match tokio::time::timeout(Duration::from_secs(5), read_frame(&mut rd))
            .await
            .map_err(|_| eyre::eyre!("the PTY holder didn't answer"))??
        {
            Some((Msg::Welcome { .. }, _)) => {}
            Some((Msg::Denied { error }, _)) => eyre::bail!("the PTY holder refused: {error}"),
            _ => eyre::bail!("the PTY holder answered something else"),
        }
        let (out, mut queue) = mpsc::unbounded_channel::<Vec<u8>>();
        let client = Arc::new(Client {
            out,
            pending: Mutex::new(HashMap::new()),
            attached: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            closed: CancellationToken::new(),
        });
        let closed = client.closed.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = closed.cancelled() => return,
                    frame = queue.recv() => match frame {
                        Some(frame) => if wr.write_all(&frame).await.is_err() { return },
                        None => return,
                    },
                }
            }
        });
        let reader = Arc::clone(&client);
        tokio::spawn(async move {
            loop {
                match read_frame(&mut rd).await {
                    Ok(Some((msg, payload))) => reader.dispatch(msg, payload),
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "PTY holder connection failed");
                        break;
                    }
                }
            }
            tracing::warn!("lost the PTY holder; its terminals are gone to this hub");
            reader.lost();
        });
        Ok(Self(client))
    }

    /// Hangs up; the holder keeps the terminals.
    pub fn close(&self) {
        self.0.closed.cancel();
    }

    pub fn is_connected(&self) -> bool {
        !self.0.closed.is_cancelled()
    }

    async fn request(&self, make: impl FnOnce(u64) -> Msg) -> Result<Msg> {
        let req = self.0.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.0.pending.lock().insert(req, tx);
        self.0.send(&make(req), &[])?;
        tokio::time::timeout(Duration::from_secs(30), rx)
            .await
            .map_err(|_| eyre::eyre!("the PTY holder didn't answer"))?
            .map_err(|_| eyre::eyre!("the PTY holder is gone"))
    }

    /// Every terminal the holder has.
    pub async fn list(&self) -> Result<Vec<Held>> {
        match self.request(|req| Msg::List { req }).await? {
            Msg::Listing { ptys, .. } => Ok(ptys),
            other => eyre::bail!("unexpected answer {other:?}"),
        }
    }

    /// Starts `spec` in the holder, tagged with `meta`, and attaches to it.
    pub async fn spawn(
        &self,
        spec: &Spawn,
        cols: u16,
        rows: u16,
        history: usize,
        meta: Value,
    ) -> Result<(Pty, oneshot::Receiver<Exit>)> {
        let id = Uuid::new_v4();
        let answer = self
            .request(|req| Msg::Spawn {
                req,
                id,
                spec: spec.clone(),
                cols,
                rows,
                history,
                meta,
            })
            .await?;
        match answer {
            Msg::Spawned { pid, .. } => Ok(self.attach(id, pid, cols, rows, history)),
            Msg::Failed { error, .. } => eyre::bail!("{error}"),
            other => eyre::bail!("unexpected answer {other:?}"),
        }
    }

    /// Takes back a terminal the holder kept: its screen comes with it.
    pub fn adopt(&self, held: &Held, history: usize) -> (Pty, oneshot::Receiver<Exit>) {
        self.attach(held.id, held.pid, held.cols, held.rows, history)
    }

    fn attach(
        &self,
        id: Uuid,
        pid: Option<u32>,
        cols: u16,
        rows: u16,
        history: usize,
    ) -> (Pty, oneshot::Receiver<Exit>) {
        let screen = Screen::new(cols, rows, history);
        let (tx, rx) = oneshot::channel();
        self.0.attached.lock().insert(
            id,
            Attached {
                screen: Arc::clone(&screen),
                history,
                exit: Some(tx),
            },
        );
        if self.0.send(&Msg::Attach { id }, &[]).is_err() {
            self.0.ended(id, None);
        }
        let io = HeldIo {
            client: Arc::clone(&self.0),
            id,
        };
        (Pty::from_parts(screen, Box::new(io), pid), rx)
    }

    /// Ends a terminal by id (one the hub didn't take back).
    pub fn kill(&self, id: Uuid) {
        let _ = self.0.send(&Msg::Kill { id }, &[]);
    }

    /// Forgets a terminal that has ended.
    pub fn reap(&self, id: Uuid) {
        let _ = self.0.send(&Msg::Reap { id }, &[]);
    }
}

/// Where Desk's helper puts the holder's socket under its run folder.
pub fn socket_in(run_dir: &Path) -> PathBuf {
    run_dir.join("pty.sock")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::pty::tests::{sh, wait_for};
    use serde_json::json;

    pub(crate) const TOKEN: &str = "a-holder-token-for-tests";

    /// A holder serving in this process on a short socket path.
    pub(crate) async fn holder() -> (tempfile::TempDir, PathBuf, CancellationToken) {
        let dir = tempfile::Builder::new()
            .prefix("h")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("run").join("pty.sock");
        let cancel = CancellationToken::new();
        tokio::spawn({
            let (socket, cancel) = (socket.clone(), cancel.clone());
            async move { serve(&socket, TOKEN, cancel).await.unwrap() }
        });
        for _ in 0..100 {
            if Holder::connect(&socket, TOKEN).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        (dir, socket, cancel)
    }

    #[tokio::test]
    async fn frames_round_trip_and_bad_ones_are_refused() {
        let frame = encode(&Msg::Output { id: Uuid::nil() }, b"\x1b[31mhi");
        let mut r = &frame[..];
        let (msg, payload) = read_frame(&mut r).await.unwrap().unwrap();
        assert!(matches!(msg, Msg::Output { id } if id.is_nil()));
        assert_eq!(payload, b"\x1b[31mhi");
        assert!(read_frame(&mut r).await.unwrap().is_none());
        let mut huge = &u32::MAX.to_be_bytes()[..];
        assert!(read_frame(&mut huge).await.is_err());
    }

    #[tokio::test]
    async fn the_wrong_token_and_a_second_holder_are_turned_away() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, socket, cancel) = holder().await;
        let err = Holder::connect(&socket, "not-the-token-at-all")
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("wrong token"), "{err}");
        let pid = std::fs::read_to_string(pid_file(&socket)).unwrap();
        assert_eq!(pid.trim(), std::process::id().to_string());
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = std::fs::metadata(socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        // A second holder on the same socket leaves the first serving.
        serve(&socket, TOKEN, CancellationToken::new())
            .await
            .unwrap();
        assert!(Holder::connect(&socket, TOKEN).await.is_ok());
        cancel.cancel();
    }

    #[tokio::test]
    async fn a_terminal_outlives_its_client_and_comes_back_with_its_screen() {
        let (_dir, socket, cancel) = holder().await;
        let first = Holder::connect(&socket, TOKEN).await.unwrap();
        let script = "stty -echo; printf '\\033[?2004h\\033[?1049hready\\n'; while read l; do echo \"got:$l\"; done";
        let (pty, _exit) = first
            .spawn(
                &sh(script),
                80,
                24,
                100,
                json!({ "kind": "terminal", "terminal": "t1" }),
            )
            .await
            .unwrap();
        let pty = Arc::new(pty);
        wait_for(&pty, "ready").await;
        pty.write(b"one\r").unwrap();
        wait_for(&pty, "got:one").await;
        let pid = pty.pid();
        first.close();
        drop(pty);

        let second = Holder::connect(&socket, TOKEN).await.unwrap();
        let held = second.list().await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(
            (held[0].pid, &held[0].meta["terminal"], held[0].exited),
            (pid, &json!("t1"), false)
        );
        let (pty, exit) = second.adopt(&held[0], 100);
        let pty = Arc::new(pty);
        let text = wait_for(&pty, "got:one").await;
        assert!(text.contains("ready"));
        let (_viewer, frames, _) = pty.attach();
        let snap = frames.iter().find_map(|f| match f {
            Frame::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        });
        assert!(snap.unwrap().contains("\x1b[?2004h"), "modes come back");
        pty.write(b"two\r").unwrap();
        wait_for(&pty, "got:two").await;
        pty.resize(100, 30).unwrap();
        pty.kill();
        tokio::time::timeout(Duration::from_secs(5), exit)
            .await
            .unwrap()
            .unwrap();
        // Seen ending by a client: forgotten.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(second.list().await.unwrap().is_empty());
        cancel.cancel();
    }

    #[tokio::test]
    async fn a_terminal_that_ends_unwatched_is_kept_until_reaped() {
        let (_dir, socket, cancel) = holder().await;
        let client = Holder::connect(&socket, TOKEN).await.unwrap();
        let (pty, _exit) = client
            .spawn(&sh("sleep 1; exit 7"), 80, 24, 10, json!({}))
            .await
            .unwrap();
        let id_pid = pty.pid();
        client.close();
        drop(pty);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let client = Holder::connect(&socket, TOKEN).await.unwrap();
        let held = client.list().await.unwrap();
        assert_eq!(
            (held[0].pid, held[0].exited, held[0].code),
            (id_pid, true, Some(7))
        );
        client.reap(held[0].id);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(client.list().await.unwrap().is_empty());
        cancel.cancel();
    }

    #[tokio::test]
    async fn the_holder_answers_queries_while_no_client_is_attached() {
        let (_dir, socket, cancel) = holder().await;
        let client = Holder::connect(&socket, TOKEN).await.unwrap();
        let script = "stty raw -echo; sleep 1; printf '\\033[c'; dd bs=1 count=7 2>/dev/null | od -An -c; sleep 30";
        let (pty, _exit) = client
            .spawn(&sh(script), 80, 24, 10, json!({}))
            .await
            .unwrap();
        let held = client.list().await.unwrap();
        client.close();
        drop(pty);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let client = Holder::connect(&socket, TOKEN).await.unwrap();
        let (pty, _exit) = client.adopt(&held[0], 10);
        wait_for(&pty, "?   1   ;   2   c").await;
        cancel.cancel();
    }

    #[tokio::test]
    async fn losing_the_holder_ends_its_terminals_for_the_hub() {
        let (_dir, socket, cancel) = holder().await;
        let client = Holder::connect(&socket, TOKEN).await.unwrap();
        let (_pty, exit) = client
            .spawn(&sh("sleep 30"), 80, 24, 10, json!({}))
            .await
            .unwrap();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), exit)
            .await
            .unwrap()
            .unwrap();
        for _ in 0..100 {
            if !client.is_connected() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the client never noticed the holder went");
    }
}

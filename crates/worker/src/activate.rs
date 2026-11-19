//! `workforce worker activate`: a scope's dev services, started on first use.
//!
//! Runs inside a light scope, in place of starting its databases at seat.
//! It holds each service's usual address (`127.0.0.1:3306`, the Docker
//! socket) and, on the first connection to one, runs the toolbox's start
//! command for that service, which brings it up on a backend address and
//! returns once it is ready. That connection, and every later one, is then
//! spliced through to the backend. A chat that never connects starts
//! nothing; a test run that does starts only what it reaches for.
//!
//! A start that fails closes the connections waiting on it and is tried
//! again by the next connection after [`RETRY_AFTER`], so a transient
//! failure does not stick and a broken service is not restarted in a loop.
//! A started service found unreachable is started again by the next
//! connection, so the start command must be idempotent.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use eyre::{bail, eyre, Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{info, warn};

/// How long a failed start is remembered before a connection tries again.
pub const RETRY_AFTER: Duration = Duration::from_secs(5);
/// The most a start command may take, dockerd and a database's first boot
/// included.
const START_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a started service's backend may refuse connections, as one does
/// for a moment after its readiness check passed.
const CONNECT_PATIENCE: Duration = Duration::from_secs(10);
const CONNECT_RETRY: Duration = Duration::from_millis(100);

/// Where a listener is held or a backend reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    Tcp(String),
    Unix(PathBuf),
}

impl std::str::FromStr for Address {
    type Err = eyre::Report;

    fn from_str(text: &str) -> Result<Self> {
        match text.split_once(':') {
            Some(("tcp", addr)) if addr.parse::<std::net::SocketAddr>().is_ok() => {
                Ok(Self::Tcp(addr.to_string()))
            }
            Some(("unix", path)) if path.starts_with('/') => Ok(Self::Unix(path.into())),
            _ => bail!("{text:?} is not tcp:<ip>:<port> or unix:/<path>"),
        }
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(addr) => write!(f, "tcp:{addr}"),
            Self::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

/// One service: its name for the start command, the address held for it,
/// and where it runs once started. Written `NAME=FRONT=BACK`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    pub name: String,
    pub front: Address,
    pub back: Address,
}

impl std::str::FromStr for Activation {
    type Err = eyre::Report;

    fn from_str(text: &str) -> Result<Self> {
        let mut parts = text.splitn(3, '=');
        let (Some(name), Some(front), Some(back)) = (parts.next(), parts.next(), parts.next())
        else {
            bail!("{text:?} is not NAME=FRONT=BACK");
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            bail!("service name {name:?} must be letters, digits, - or _");
        }
        let activation = Self {
            name: name.to_string(),
            front: front.parse()?,
            back: back.parse()?,
        };
        if activation.front == activation.back {
            bail!("{name}: the address held and the backend are both {front}");
        }
        Ok(activation)
    }
}

/// What a service's start has come to.
enum Started {
    Not,
    Yes,
    Failed(Instant),
}

/// One service's start, run at most once at a time and kept once it
/// succeeds.
struct Service {
    activation: Activation,
    start: PathBuf,
    state: Mutex<Started>,
}

impl Service {
    /// Start the service unless it is started; waiters on a start in flight
    /// share its outcome.
    async fn ensure(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        match *state {
            Started::Yes => return Ok(()),
            Started::Failed(at) if at.elapsed() < RETRY_AFTER => {
                bail!(
                    "{} failed to start {}s ago",
                    self.activation.name,
                    at.elapsed().as_secs()
                )
            }
            _ => {}
        }
        let began = Instant::now();
        info!(service = %self.activation.name, "first use; starting");
        match run_start(&self.start, &self.activation.name).await {
            Ok(()) => {
                info!(service = %self.activation.name, secs = began.elapsed().as_secs(), "started");
                *state = Started::Yes;
                Ok(())
            }
            Err(error) => {
                *state = Started::Failed(Instant::now());
                Err(error)
            }
        }
    }

    /// Serve one connection to the held address: start the service, then
    /// splice through to it.
    async fn serve<S>(&self, mut client: S)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let name = &self.activation.name;
        if let Err(error) = self.ensure().await {
            warn!(service = %name, error = %format!("{error:#}"), "not started; closing the connection");
            return;
        }
        let mut backend = match connect(&self.activation.back).await {
            Ok(backend) => backend,
            Err(error) => {
                // It died since, or never came up where it said: the next
                // connection runs the start again, which brings back what
                // is not running and leaves what is.
                warn!(service = %name, error = %format!("{error:#}"), "started but unreachable; starting it again on the next connection");
                *self.state.lock().await = Started::Not;
                return;
            }
        };
        let _ = tokio::io::copy_bidirectional(&mut client, &mut backend).await;
    }
}

/// Run `start NAME`, which must bring the service up and return once it is
/// ready, within [`START_TIMEOUT`].
async fn run_start(start: &std::path::Path, name: &str) -> Result<()> {
    let mut command = tokio::process::Command::new(start);
    command.arg(name).kill_on_drop(true);
    let output = tokio::time::timeout(START_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            eyre!(
                "{} {name} took over {}s",
                start.display(),
                START_TIMEOUT.as_secs()
            )
        })?
        .with_context(|| format!("running {} {name}", start.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{} {name} failed ({}): {}",
            start.display(),
            output.status,
            stderr.trim().lines().last().unwrap_or_default()
        );
    }
    Ok(())
}

/// A connection to a backend, either kind.
enum Backend {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl AsyncRead for Backend {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Backend {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// Connect to a started backend, waiting out a moment of refusals.
async fn connect(back: &Address) -> Result<Backend> {
    let deadline = Instant::now() + CONNECT_PATIENCE;
    loop {
        let attempt = match back {
            Address::Tcp(addr) => TcpStream::connect(addr).await.map(Backend::Tcp),
            Address::Unix(path) => UnixStream::connect(path).await.map(Backend::Unix),
        };
        match attempt {
            Ok(backend) => return Ok(backend),
            Err(_) if Instant::now() + CONNECT_RETRY < deadline => {
                tokio::time::sleep(CONNECT_RETRY).await
            }
            Err(error) => return Err(error).with_context(|| format!("connecting to {back}")),
        }
    }
}

/// A held address, bound.
enum Held {
    Tcp(TcpListener),
    Unix(UnixListener),
}

async fn hold(front: &Address) -> Result<Held> {
    match front {
        Address::Tcp(addr) => Ok(Held::Tcp(
            TcpListener::bind(addr)
                .await
                .with_context(|| format!("holding {front}"))?,
        )),
        Address::Unix(path) => {
            // A socket left by a previous run of the scope's init.
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("clearing {}", path.display()))
                }
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            Ok(Held::Unix(
                UnixListener::bind(path).with_context(|| format!("holding {front}"))?,
            ))
        }
    }
}

/// Hold every activation's address, say so by creating `ready` once all are
/// held, and serve until the process is stopped. Fails at once when any
/// address cannot be held.
pub async fn activate(
    start: PathBuf,
    ready: Option<PathBuf>,
    activations: Vec<Activation>,
) -> Result<()> {
    if activations.is_empty() {
        bail!("nothing to activate");
    }
    let mut names = std::collections::HashSet::new();
    for activation in &activations {
        if !names.insert(activation.name.as_str()) {
            bail!("{} is named twice", activation.name);
        }
    }
    let mut held = Vec::with_capacity(activations.len());
    for activation in activations {
        let listener = hold(&activation.front).await?;
        info!(service = %activation.name, front = %activation.front, back = %activation.back, "held until first use");
        held.push((
            Arc::new(Service {
                activation,
                start: start.clone(),
                state: Mutex::new(Started::Not),
            }),
            listener,
        ));
    }
    if let Some(ready) = ready {
        std::fs::write(&ready, b"").with_context(|| format!("creating {}", ready.display()))?;
    }
    let loops = held
        .into_iter()
        .map(|(service, listener)| accept_forever(service, listener));
    futures::future::join_all(loops).await;
    Ok(())
}

async fn accept_forever(service: Arc<Service>, listener: Held) {
    loop {
        let accepted = match &listener {
            Held::Tcp(listener) => listener.accept().await.map(|(stream, _)| {
                let service = Arc::clone(&service);
                tokio::spawn(async move { service.serve(stream).await });
            }),
            Held::Unix(listener) => listener.accept().await.map(|(stream, _)| {
                let service = Arc::clone(&service);
                tokio::spawn(async move { service.serve(stream).await });
            }),
        };
        if let Err(error) = accepted {
            // Out of descriptors, or a connection reset before accept: keep
            // the address held and try again shortly.
            warn!(service = %service.activation.name, %error, "accepting a connection");
            tokio::time::sleep(CONNECT_RETRY).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn activations_parse_and_refuse_what_cannot_work() {
        let parsed: Activation = "mysql=tcp:127.0.0.1:3306=tcp:127.0.0.1:13306"
            .parse()
            .unwrap();
        assert_eq!(parsed.name, "mysql");
        assert_eq!(parsed.front, Address::Tcp("127.0.0.1:3306".into()));
        let docker: Activation = "docker=unix:/var/run/docker.sock=unix:/run/toolbox/docker.sock"
            .parse()
            .unwrap();
        assert_eq!(
            docker.back,
            Address::Unix("/run/toolbox/docker.sock".into())
        );
        for bad in [
            "mysql",
            "mysql=tcp:127.0.0.1:3306",
            "my sql=tcp:127.0.0.1:3306=tcp:127.0.0.1:13306",
            "mysql=tcp:localhost=tcp:127.0.0.1:13306",
            "mysql=unix:relative=tcp:127.0.0.1:13306",
            "mysql=tcp:127.0.0.1:3306=tcp:127.0.0.1:3306",
            "=tcp:127.0.0.1:3306=tcp:127.0.0.1:13306",
        ] {
            assert!(bad.parse::<Activation>().is_err(), "{bad}");
        }
    }

    /// A start command that records each start, then runs `then`.
    fn start_script(dir: &std::path::Path, then: &str) -> PathBuf {
        let path = dir.join("start");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$1\" >> {}\n{then}\n",
                dir.join("starts").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn starts(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("starts"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// An echo server on `addr` that upper-cases what it reads.
    async fn backend(addr: String) {
        let listener = TcpListener::bind(&addr).await.unwrap();
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let upper = buf[..n].to_ascii_uppercase();
                    if stream.write_all(&upper).await.is_err() {
                        break;
                    }
                }
            });
        }
    }

    async fn wait_for(path: &std::path::Path) {
        for _ in 0..200 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{} never appeared", path.display());
    }

    async fn round_trip<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, text: &[u8]) -> Vec<u8> {
        stream.write_all(text).await.unwrap();
        let mut buf = vec![0u8; text.len()];
        stream.read_exact(&mut buf).await.unwrap();
        buf
    }

    #[tokio::test]
    async fn nothing_starts_until_a_connection_and_then_once_for_all_of_them() {
        let dir = tempfile::tempdir().unwrap();
        let (front, back) = (free_port(), free_port());
        // The start brings the backend up only when it runs, as the toolbox's
        // does, so a splice that skipped the start would find nothing there.
        let marker = dir.path().join("may-serve");
        let start = start_script(
            dir.path(),
            &format!("sleep 0.2; touch {}", marker.display()),
        );
        let back_addr = format!("127.0.0.1:{back}");
        let marker_seen = marker.clone();
        tokio::spawn(async move {
            wait_for(&marker_seen).await;
            backend(back_addr).await
        });
        let ready = dir.path().join("ready");
        let activation: Activation = format!("db=tcp:127.0.0.1:{front}=tcp:127.0.0.1:{back}")
            .parse()
            .unwrap();
        tokio::spawn(activate(start, Some(ready.clone()), vec![activation]));
        wait_for(&ready).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(starts(dir.path()).is_empty(), "held, not started");

        let connections = (0..5).map(|i| async move {
            let stream = TcpStream::connect(("127.0.0.1", front)).await.unwrap();
            round_trip(stream, format!("hello {i}").as_bytes()).await
        });
        let answers = futures::future::join_all(connections).await;
        for (i, answer) in answers.iter().enumerate() {
            assert_eq!(answer, format!("HELLO {i}").as_bytes());
        }
        let later = TcpStream::connect(("127.0.0.1", front)).await.unwrap();
        assert_eq!(round_trip(later, b"again").await, b"AGAIN");
        assert_eq!(starts(dir.path()), ["db"], "one start for every connection");
    }

    #[tokio::test]
    async fn a_failed_start_closes_its_connection_and_is_tried_again_later() {
        let dir = tempfile::tempdir().unwrap();
        let (front, back) = (free_port(), free_port());
        let fail_once = dir.path().join("failed");
        let start = start_script(
            dir.path(),
            &format!(
                "if [ ! -f {f} ]; then touch {f}; echo 'mysqld: no space' >&2; exit 1; fi",
                f = fail_once.display()
            ),
        );
        tokio::spawn(backend(format!("127.0.0.1:{back}")));
        let activation: Activation = format!("db=tcp:127.0.0.1:{front}=tcp:127.0.0.1:{back}")
            .parse()
            .unwrap();
        let ready = dir.path().join("ready");
        tokio::spawn(activate(start, Some(ready.clone()), vec![activation]));
        wait_for(&ready).await;

        let mut first = TcpStream::connect(("127.0.0.1", front)).await.unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(
            first.read(&mut buf).await.unwrap_or(0),
            0,
            "a failed start closes the connection"
        );
        let mut soon = TcpStream::connect(("127.0.0.1", front)).await.unwrap();
        assert_eq!(soon.read(&mut buf).await.unwrap_or(0), 0);
        assert_eq!(starts(dir.path()), ["db"], "not retried within the pause");

        tokio::time::sleep(RETRY_AFTER).await;
        let later = TcpStream::connect(("127.0.0.1", front)).await.unwrap();
        assert_eq!(round_trip(later, b"ok").await, b"OK");
        assert_eq!(starts(dir.path()), ["db", "db"]);
    }

    #[tokio::test]
    async fn a_started_service_that_died_is_started_again_by_the_next_connection() {
        let dir = tempfile::tempdir().unwrap();
        let (front, back) = (free_port(), free_port());
        let start = start_script(dir.path(), "true");
        let activation: Activation = format!("db=tcp:127.0.0.1:{front}=tcp:127.0.0.1:{back}")
            .parse()
            .unwrap();
        let ready = dir.path().join("ready");
        let serving = tokio::spawn(backend(format!("127.0.0.1:{back}")));
        tokio::spawn(activate(start, Some(ready.clone()), vec![activation]));
        wait_for(&ready).await;
        let first = TcpStream::connect(("127.0.0.1", front)).await.unwrap();
        assert_eq!(round_trip(first, b"a").await, b"A");

        serving.abort();
        let _ = serving.await;
        let mut gone = TcpStream::connect(("127.0.0.1", front)).await.unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(gone.read(&mut buf).await.unwrap_or(0), 0);
        assert_eq!(starts(dir.path()), ["db"]);

        tokio::spawn(backend(format!("127.0.0.1:{back}")));
        let back_again = TcpStream::connect(("127.0.0.1", front)).await.unwrap();
        assert_eq!(round_trip(back_again, b"b").await, b"B");
        assert_eq!(starts(dir.path()), ["db", "db"]);
    }

    #[tokio::test]
    async fn a_unix_socket_is_held_in_place_of_a_stale_one_and_spliced_to_tcp() {
        let dir = tempfile::tempdir().unwrap();
        let back = free_port();
        let socket = dir.path().join("run/docker.sock");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        std::fs::write(&socket, b"stale").unwrap();
        tokio::spawn(backend(format!("127.0.0.1:{back}")));
        let start = start_script(dir.path(), "true");
        let activation: Activation =
            format!("docker=unix:{}=tcp:127.0.0.1:{back}", socket.display())
                .parse()
                .unwrap();
        let ready = dir.path().join("ready");
        tokio::spawn(activate(start, Some(ready.clone()), vec![activation]));
        wait_for(&ready).await;
        let stream = UnixStream::connect(&socket).await.unwrap();
        assert_eq!(round_trip(stream, b"info").await, b"INFO");
        assert_eq!(starts(dir.path()), ["docker"]);
    }

    #[tokio::test]
    async fn an_address_that_cannot_be_held_fails_at_once_and_says_which() {
        let dir = tempfile::tempdir().unwrap();
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let activation: Activation = format!("db=tcp:127.0.0.1:{port}=tcp:127.0.0.1:1")
            .parse()
            .unwrap();
        let error = activate(start_script(dir.path(), "true"), None, vec![activation])
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains(&format!("tcp:127.0.0.1:{port}")),
            "{error:#}"
        );
        let twice: Vec<Activation> = [
            "db=tcp:127.0.0.1:1=tcp:127.0.0.1:2",
            "db=tcp:127.0.0.1:3=tcp:127.0.0.1:4",
        ]
        .iter()
        .map(|a| a.parse().unwrap())
        .collect();
        assert!(activate(PathBuf::from("/bin/true"), None, twice)
            .await
            .is_err());
    }
}

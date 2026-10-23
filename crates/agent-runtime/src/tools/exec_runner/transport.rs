//! Where the runner daemon is and how to reach it: the hub-local unix socket
//! authenticated by peer uid, or a remote host authenticated by mutual TLS.
//! Everything above this module speaks the same frames over either.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use eyre::{Context, Result};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::{info, warn};
use workforce_common::settings::ExecConfig;

use super::tls;

/// Socket permission bits: owner+group read/write, no world access. The init
/// places the socket in group `workforce` so only `hub` (the sole group member
/// that should connect) can reach it; `SO_PEERCRED` is the authoritative check.
const SOCKET_MODE: u32 = 0o660;

/// Idle time before the first keepalive probe, the gap between probes, and
/// how many may go unanswered: a half-open TCP peer is declared dead in about
/// the same window the frame-level heartbeat gives the client.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(10);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEPALIVE_RETRIES: u32 = 3;

/// Time allowed for the TCP connect and, separately, the TLS handshake, so a
/// black-holed host fails within the request's own deadline and a stalled
/// peer cannot hold a session task open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// PEM-encoded material for one side of a mutually authenticated link.
#[derive(Clone, PartialEq, Eq)]
pub struct TlsIdentity {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
}

impl std::fmt::Debug for TlsIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsIdentity")
            .field("ca_pem", &self.ca_pem.len())
            .field("cert_pem", &self.cert_pem.len())
            .field("key_pem", &"<redacted>")
            .finish()
    }
}

/// A remote daemon reached over mutual TLS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsEndpoint {
    pub addr: String,
    pub server_name: String,
    pub identity: TlsIdentity,
}

/// The daemon a client dispatches to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerEndpoint {
    Unix(PathBuf),
    Tls(TlsEndpoint),
}

impl RunnerEndpoint {
    /// The hub-local runner `[exec] runner_socket` names, or `None` when
    /// commands run in-process.
    pub fn from_config(config: &ExecConfig) -> Option<Self> {
        config.runner_socket.clone().map(Self::Unix)
    }

    /// The worker `[exec.remote]` names, or `None` when there is none. It
    /// serves scoped work only, so it never stands in for the local runner.
    pub fn remote_from_config(config: &ExecConfig) -> Option<Self> {
        config.remote.as_ref().map(|remote| {
            Self::Tls(TlsEndpoint {
                addr: remote.addr.clone(),
                server_name: remote.server_name.clone(),
                identity: TlsIdentity {
                    ca_pem: remote.ca_pem.clone(),
                    cert_pem: remote.cert_pem.clone(),
                    key_pem: remote.key_pem.clone(),
                },
            })
        })
    }

    /// The lowest protocol revision a daemon at this endpoint must speak,
    /// whatever the request. Remote daemons began with scopes, so that is the
    /// floor; above it each request asks for what it uses, and a worker that
    /// rolls after its hub keeps serving everything it already supports.
    pub fn minimum_protocol(&self) -> u32 {
        match self {
            Self::Unix(_) => 0,
            Self::Tls(_) => super::protocol::PROTOCOL_SCOPES,
        }
    }
}

impl std::fmt::Display for RunnerEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "unix socket {}", path.display()),
            Self::Tls(endpoint) => write!(f, "{} ({})", endpoint.addr, endpoint.server_name),
        }
    }
}

/// Where a daemon listens and who it admits.
#[derive(Debug, Clone)]
pub enum Listener {
    Unix {
        path: PathBuf,
        accept_uid: u32,
    },
    Tls {
        addr: SocketAddr,
        identity: TlsIdentity,
        accept_common_name: String,
    },
}

/// A bidirectional byte stream between a client and a daemon.
pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

pub type Connection = Box<dyn Duplex>;

/// Open a connection to `endpoint`, authenticating the daemon when the link
/// is TLS. Fail-closed wording, because every caller surfaces this verbatim.
pub async fn connect(endpoint: &RunnerEndpoint) -> Result<Connection> {
    let unavailable = || format!("secure command runner unavailable: cannot connect to {endpoint}");
    match endpoint {
        RunnerEndpoint::Unix(path) => {
            let stream = UnixStream::connect(path).await.with_context(unavailable)?;
            Ok(Box::new(stream))
        }
        RunnerEndpoint::Tls(remote) => {
            let config = tls::client_config(&remote.identity).with_context(unavailable)?;
            let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&remote.addr))
                .await
                .map_err(|_| eyre::eyre!("TCP connect timed out"))
                .and_then(|connected| connected.map_err(Into::into))
                .with_context(unavailable)?;
            keepalive(&tcp).with_context(unavailable)?;
            let server_name = rustls::pki_types::ServerName::try_from(remote.server_name.clone())
                .with_context(unavailable)?;
            let stream = tokio::time::timeout(
                CONNECT_TIMEOUT,
                TlsConnector::from(config).connect(server_name, tcp),
            )
            .await
            .map_err(|_| eyre::eyre!("TLS handshake timed out"))
            .and_then(|handshake| handshake.map_err(Into::into))
            .with_context(unavailable)?;
            Ok(Box::new(stream))
        }
    }
}

fn keepalive(stream: &TcpStream) -> Result<()> {
    let keepalive = TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL)
        .with_retries(KEEPALIVE_RETRIES);
    SockRef::from(stream)
        .set_tcp_keepalive(&keepalive)
        .context("enabling TCP keepalive")
}

/// A bound listener that hands out connections still awaiting authentication.
pub struct Acceptor {
    bound: Bound,
}

enum Bound {
    Unix {
        listener: UnixListener,
        accept_uid: u32,
    },
    Tls {
        listener: TcpListener,
        acceptor: TlsAcceptor,
        accept_common_name: String,
    },
}

/// A connection accepted but not yet authenticated. Authentication runs on
/// the session task, not the accept loop, so one slow peer cannot stall the
/// daemon for everyone.
pub enum Incoming {
    Unix {
        stream: UnixStream,
        accept_uid: u32,
    },
    Tls {
        stream: TcpStream,
        acceptor: TlsAcceptor,
        accept_common_name: String,
    },
}

impl Acceptor {
    pub async fn bind(listener: Listener) -> Result<Self> {
        let bound = match listener {
            Listener::Unix { path, accept_uid } => {
                if path.exists() {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("removing stale socket {}", path.display()))?;
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating socket dir {}", parent.display()))?;
                }
                let listener = UnixListener::bind(&path)
                    .with_context(|| format!("binding unix socket {}", path.display()))?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(SOCKET_MODE))
                    .with_context(|| format!("chmod {SOCKET_MODE:o} on {}", path.display()))?;
                info!(socket = %path.display(), accept_uid, "exec runner listening");
                Bound::Unix {
                    listener,
                    accept_uid,
                }
            }
            Listener::Tls {
                addr,
                identity,
                accept_common_name,
            } => {
                let acceptor = TlsAcceptor::from(tls::server_config(&identity)?);
                let listener = TcpListener::bind(addr)
                    .await
                    .with_context(|| format!("binding {addr}"))?;
                info!(addr = %listener.local_addr()?, accept_common_name, "exec runner listening");
                Bound::Tls {
                    listener,
                    acceptor,
                    accept_common_name,
                }
            }
        };
        Ok(Self { bound })
    }

    /// The bound TCP address, for a listener bound to an ephemeral port.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        match &self.bound {
            Bound::Tls { listener, .. } => listener.local_addr().ok(),
            Bound::Unix { .. } => None,
        }
    }

    pub async fn accept(&self) -> Result<Incoming> {
        match &self.bound {
            Bound::Unix {
                listener,
                accept_uid,
            } => {
                let (stream, _) = listener.accept().await?;
                Ok(Incoming::Unix {
                    stream,
                    accept_uid: *accept_uid,
                })
            }
            Bound::Tls {
                listener,
                acceptor,
                accept_common_name,
            } => {
                let (stream, _) = listener.accept().await?;
                Ok(Incoming::Tls {
                    stream,
                    acceptor: acceptor.clone(),
                    accept_common_name: accept_common_name.clone(),
                })
            }
        }
    }
}

impl Incoming {
    /// Authenticate the peer, returning `None` for one that is not admitted.
    pub async fn authenticate(self) -> Result<Option<Connection>> {
        match self {
            Self::Unix { stream, accept_uid } => {
                let peer_uid = stream
                    .peer_cred()
                    .context("reading peer credentials")?
                    .uid();
                if peer_uid != accept_uid {
                    warn!(peer_uid, accept_uid, "rejecting unauthorised peer");
                    return Ok(None);
                }
                Ok(Some(Box::new(stream)))
            }
            Self::Tls {
                stream,
                acceptor,
                accept_common_name,
            } => {
                keepalive(&stream)?;
                let stream =
                    match tokio::time::timeout(CONNECT_TIMEOUT, acceptor.accept(stream)).await {
                        Ok(Ok(stream)) => stream,
                        Ok(Err(e)) => {
                            warn!(error = %e, "rejecting peer that failed the TLS handshake");
                            return Ok(None);
                        }
                        Err(_) => {
                            warn!("rejecting peer that stalled the TLS handshake");
                            return Ok(None);
                        }
                    };
                let presented = stream
                    .get_ref()
                    .1
                    .peer_certificates()
                    .and_then(|chain| chain.first())
                    .map(tls::common_name)
                    .transpose()?
                    .flatten();
                if presented.as_deref() != Some(accept_common_name.as_str()) {
                    warn!(
                        ?presented,
                        accept_common_name, "rejecting unauthorised peer"
                    );
                    return Ok(None);
                }
                Ok(Some(Box::new(stream)))
            }
        }
    }
}

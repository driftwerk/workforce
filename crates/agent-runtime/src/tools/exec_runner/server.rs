//! Runner daemon: accept connections, authenticate each peer, and run one
//! request per connection by streaming frames back. Runs as an unprivileged
//! user so the commands it executes cannot read the hub user's vault, env, or
//! `/proc`.

use std::sync::Arc;
use std::time::Duration;

use eyre::{bail, Result};
use tokio::io::ReadHalf;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::handler::JobHandler;
use super::link::{JobLink, McpBridge};
use super::protocol::{
    read_framed, write_framed, ClientFrame, Request, ServerFrame, PROTOCOL_VERSION,
};
use super::transport::{Acceptor, Connection, Incoming, Listener};

/// How often a running job's connection carries a heartbeat, so a client can
/// tell a silent job from a dead daemon.
const HEARTBEAT: Duration = Duration::from_secs(10);

/// Stdin chunks an interactive job may have waiting. A client running this
/// far ahead of the command means the command stopped reading, and the job
/// is ended rather than left to block the connection.
const STDIN_BACKLOG: usize = 256;

/// Bind `listener` and serve requests with `handler` until the listener
/// errors unrecoverably (the supervising init restarts the daemon).
pub async fn serve(listener: Listener, handler: Arc<dyn JobHandler>) -> Result<()> {
    serve_on(Acceptor::bind(listener).await?, handler).await
}

/// Serve requests on an already bound acceptor.
pub async fn serve_on(acceptor: Acceptor, handler: Arc<dyn JobHandler>) -> Result<()> {
    loop {
        let incoming = match acceptor.accept().await {
            Ok(incoming) => incoming,
            Err(e) => {
                warn!(error = %e, "accept failed");
                continue;
            }
        };
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            if let Err(e) = session(incoming, handler).await {
                warn!(error = %e, "connection handler error");
            }
        });
    }
}

async fn session(incoming: Incoming, handler: Arc<dyn JobHandler>) -> Result<()> {
    match incoming.authenticate().await? {
        Some(connection) => drive(connection, handler).await,
        None => Ok(()),
    }
}

/// Read one request, announce the protocol, run the job, and relay frames in
/// both directions until the job ends or the peer goes away.
async fn drive(connection: Connection, handler: Arc<dyn JobHandler>) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(connection);
    let request = match read_framed::<_, ClientFrame>(&mut reader).await? {
        Some(ClientFrame::Request(request)) => *request,
        Some(_) => bail!("the first frame must be a request"),
        None => return Ok(()),
    };
    write_framed(
        &mut writer,
        &ServerFrame::Ready {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await?;

    let (frames_tx, mut frames_rx) = mpsc::channel::<ServerFrame>(64);
    let (asked, gone) = (CancellationToken::new(), CancellationToken::new());
    let link = JobLink::new(frames_tx, asked.clone(), gone.clone());
    let (link, stdin) = match request {
        Request::Interactive(_) => {
            let (stdin_tx, stdin_rx) = mpsc::channel(STDIN_BACKLOG);
            (link.with_stdin(stdin_rx), Some(stdin_tx))
        }
        _ => (link, None),
    };
    let bridge = link.bridge();
    let mut job = Job::spawn(handler, request, link, gone);
    let peer = read_client_frames(reader, bridge, stdin, asked);
    tokio::pin!(peer);
    let mut heartbeat =
        tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);

    loop {
        tokio::select! {
            biased;
            Some(frame) = frames_rx.recv() => write_framed(&mut writer, &frame).await?,
            _ = heartbeat.tick() => write_framed(&mut writer, &ServerFrame::Heartbeat).await?,
            terminal = &mut job.handle => {
                while let Some(frame) = frames_rx.recv().await {
                    write_framed(&mut writer, &frame).await?;
                }
                let terminal = terminal
                    .map_err(eyre::Report::from)
                    .and_then(|frame| frame)
                    .unwrap_or_else(|e| {
                        warn!(error = %format!("{e:#}"), "a request failed");
                        ServerFrame::Failed { message: format!("{e:#}") }
                    });
                write_framed(&mut writer, &terminal).await?;
                return Ok(());
            }
            _ = &mut peer => {
                debug!("client disconnected; ending the job");
                return Ok(());
            }
        }
    }
}

/// The spawned job, told its client is gone and aborted on every exit path so
/// no route out of [`drive`] leaves a connection-bound command running.
struct Job {
    handle: JoinHandle<Result<ServerFrame>>,
    gone: CancellationToken,
}

impl Job {
    fn spawn(
        handler: Arc<dyn JobHandler>,
        request: Request,
        link: JobLink,
        gone: CancellationToken,
    ) -> Self {
        let handle = tokio::spawn(async move { handler.run(request, link).await });
        Self { handle, gone }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.gone.cancel();
        self.handle.abort();
    }
}

/// Relay what the client sends after its request: a cancel, replies to the
/// job's tool calls, or an interactive job's input. Returns when the client
/// is gone. Never waits on the job, so a cancel or a hang-up is always read.
async fn read_client_frames(
    mut reader: ReadHalf<Connection>,
    bridge: Arc<McpBridge>,
    mut stdin: Option<mpsc::Sender<String>>,
    cancel: CancellationToken,
) {
    loop {
        match read_framed::<_, ClientFrame>(&mut reader).await {
            Ok(Some(ClientFrame::Cancel)) => cancel.cancel(),
            Ok(Some(ClientFrame::McpResponse { id, body })) => bridge.fulfil(id, body),
            Ok(Some(ClientFrame::Stdin { chunk })) => {
                match stdin.as_ref().map(|input| input.try_send(chunk)) {
                    Some(Ok(())) => {}
                    Some(Err(TrySendError::Full(_))) => {
                        warn!("an interactive command stopped reading its input; ending it");
                        stdin = None;
                        cancel.cancel();
                    }
                    Some(Err(TrySendError::Closed(_))) => stdin = None,
                    None => debug!("input for a job that takes none, or no more; dropped"),
                }
            }
            Ok(Some(ClientFrame::CloseStdin)) => stdin = None,
            Ok(Some(ClientFrame::Request(_))) => {
                warn!("client sent a second request on one connection; ignoring it")
            }
            Ok(None) | Err(_) => return,
        }
    }
}

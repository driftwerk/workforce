//! Dispatch client: connect to the runner daemon, send one request, forward
//! the output stream to a sink, answer the daemon's tool calls, and return the
//! terminal frame.
//!
//! Fail-closed invariant: any connect/IO/EOF-before-terminal error returns
//! `Err`. The caller MUST surface that as a failure and MUST NEVER fall back to
//! in-process execution — doing so would run the agent's command back in the
//! hub user, re-exposing the vault the executor exists to protect.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use eyre::{bail, Result};
use futures::future::BoxFuture;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use super::protocol::{
    frame_body, read_framed, write_frame_body, write_framed, ClientFrame, ExecRequest, ExecSummary,
    InteractiveRequest, JobId, Offsets, Prepared, RefusalReason, Request, RunRequest, ScopeId,
    ServerFrame, Stream, WorkerHealth, PROTOCOL_SCOPES,
};
use super::transport::{connect, Connection, RunnerEndpoint};

/// Grace added to the request timeout for the client-side read deadline, so the
/// daemon (which authoritatively enforces `timeout_secs`) always wins the race
/// and no child is orphaned.
const CLIENT_READ_GRACE_SECS: u64 = 10;

/// The link to the daemon broke before the request ended. The daemon may still
/// be running it; a durable job can be attached to again.
#[derive(Debug)]
pub struct LinkLost(String);

impl std::fmt::Display for LinkLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LinkLost {}

fn link_lost(message: impl Into<String>) -> eyre::Report {
    eyre::Report::new(LinkLost(message.into()))
}

/// The first and longest pause between attempts to reach a durable job again.
const REATTACH_PAUSE_MIN: Duration = Duration::from_secs(1);
const REATTACH_PAUSE_MAX: Duration = Duration::from_secs(10);
/// How long a job waits for the one before it in its scope to finish ending.
const SCOPE_BUSY_WAIT: Duration = Duration::from_secs(60);
/// How long an attach made only to cancel a job may take.
const CANCEL_TIMEOUT_SECS: u64 = 30;

/// Answers the daemon's MCP requests with the body to send back.
pub type McpResponder = Arc<dyn Fn(String) -> BoxFuture<'static, String> + Send + Sync>;

/// What a daemon told one connection about the protocol it speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolEvent {
    /// It announced a protocol the request can use.
    Announced(u32),
    /// It announced an older protocol than the request needs, or none.
    TooOld {
        announced: Option<u32>,
        required: u32,
    },
}

/// Told of every [`ProtocolEvent`] on every connection, with its endpoint.
pub type ProtocolObserver = Arc<dyn Fn(&RunnerEndpoint, ProtocolEvent) + Send + Sync>;

static PROTOCOL_OBSERVER: OnceLock<ProtocolObserver> = OnceLock::new();

/// Install the process's protocol observer. The first one installed stays.
pub fn observe_protocol(observer: ProtocolObserver) {
    let _ = PROTOCOL_OBSERVER.set(observer);
}

fn report_protocol(endpoint: &RunnerEndpoint, event: ProtocolEvent) {
    if let Some(observer) = PROTOCOL_OBSERVER.get() {
        observer(endpoint, event);
    }
}

/// How long the client tolerates silence and how long it waits for the daemon
/// to acknowledge a cancel.
#[derive(Debug, Clone, Copy)]
pub struct Liveness {
    pub silence: Duration,
    pub cancel_grace: Duration,
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            silence: Duration::from_secs(40),
            cancel_grace: Duration::from_secs(15),
        }
    }
}

/// The terminal frame of a dispatched request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done(ExecSummary),
    Prepared(Prepared),
    Healthy(WorkerHealth),
    Refused(RefusalReason),
}

impl Outcome {
    fn describe(&self) -> &'static str {
        match self {
            Self::Done(_) => "a command summary",
            Self::Prepared(_) => "a prepared scope",
            Self::Healthy(_) => "a health report",
            Self::Refused(_) => "a refusal",
        }
    }
}

/// Run `request` on the daemon at `endpoint` with the default liveness.
pub async fn dispatch(
    endpoint: &RunnerEndpoint,
    request: Request,
    sink: mpsc::Sender<ServerFrame>,
    cancel: CancellationToken,
    mcp: Option<McpResponder>,
) -> Result<Outcome> {
    dispatch_with(endpoint, request, sink, cancel, mcp, Liveness::default()).await
}

/// Run a command and return its summary; any other terminal frame is an
/// error, as is a refusal.
pub async fn dispatch_exec(
    endpoint: &RunnerEndpoint,
    request: ExecRequest,
    sink: mpsc::Sender<ServerFrame>,
    cancel: CancellationToken,
    mcp: Option<McpResponder>,
) -> Result<ExecSummary> {
    match dispatch(endpoint, Request::Exec(request), sink, cancel, mcp).await? {
        Outcome::Done(summary) => Ok(summary),
        Outcome::Refused(reason) => bail!("secure command runner refused the command: {reason}"),
        other => bail!(
            "secure command runner answered a command with {}",
            other.describe()
        ),
    }
}

/// Run `request` on the daemon at `endpoint`, forwarding output chunks to
/// `sink` and returning the terminal frame. Errors are fail-closed: the caller
/// turns them into a failure and never executes locally.
pub async fn dispatch_with(
    endpoint: &RunnerEndpoint,
    request: Request,
    sink: mpsc::Sender<ServerFrame>,
    cancel: CancellationToken,
    mcp: Option<McpResponder>,
    liveness: Liveness,
) -> Result<Outcome> {
    dispatch_streaming(endpoint, request, sink, None, cancel, mcp, liveness).await
}

/// Stdin bytes one frame carries at most, well under the frame cap.
const STDIN_CHUNK: usize = 64 * 1024;

/// Run an interactive command on the daemon: what arrives on `stdin` is
/// written to it, in frames of at most [`STDIN_CHUNK`] bytes, and its stdin
/// closes when `stdin` ends. Output streams to `sink` until it exits.
pub async fn dispatch_interactive(
    endpoint: &RunnerEndpoint,
    request: InteractiveRequest,
    sink: mpsc::Sender<ServerFrame>,
    stdin: mpsc::Receiver<String>,
    cancel: CancellationToken,
) -> Result<ExecSummary> {
    let request = Request::Interactive(request);
    match dispatch_streaming(
        endpoint,
        request,
        sink,
        Some(stdin),
        cancel,
        None,
        Liveness::default(),
    )
    .await?
    {
        Outcome::Done(summary) => Ok(summary),
        Outcome::Refused(reason) => bail!("secure command runner refused the command: {reason}"),
        other => bail!(
            "secure command runner answered a command with {}",
            other.describe()
        ),
    }
}

/// Forward `stdin` as frames, then say it ended.
async fn forward_stdin(mut stdin: mpsc::Receiver<String>, out: mpsc::Sender<ClientFrame>) {
    while let Some(text) = stdin.recv().await {
        for chunk in chunks(&text, STDIN_CHUNK) {
            let frame = ClientFrame::Stdin {
                chunk: chunk.to_string(),
            };
            if out.send(frame).await.is_err() {
                return;
            }
        }
    }
    let _ = out.send(ClientFrame::CloseStdin).await;
}

/// `text` in pieces of at most `max` bytes, each cut on a character boundary.
fn chunks(text: &str, max: usize) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut at = rest.len().min(max);
        while !rest.is_char_boundary(at) {
            at -= 1;
        }
        let (piece, tail) = rest.split_at(at);
        rest = tail;
        Some(piece)
    })
}

async fn dispatch_streaming(
    endpoint: &RunnerEndpoint,
    request: Request,
    sink: mpsc::Sender<ServerFrame>,
    stdin: Option<mpsc::Receiver<String>>,
    cancel: CancellationToken,
    mcp: Option<McpResponder>,
    liveness: Liveness,
) -> Result<Outcome> {
    let deadline = Duration::from_secs(request.timeout_secs() + CLIENT_READ_GRACE_SECS);
    let required = request.required_protocol().max(endpoint.minimum_protocol());
    let body = frame_body(&ClientFrame::Request(Box::new(request)))?;

    let connection = connect(endpoint)
        .await
        .map_err(|e| link_lost(format!("{e:#}")))?;
    let (reader, mut writer) = tokio::io::split(connection);
    write_frame_body(&mut writer, &body).await.map_err(|e| {
        link_lost(format!(
            "secure command runner unavailable: failed to send request: {e}"
        ))
    })?;

    let (out_tx, out_rx) = mpsc::channel::<ClientFrame>(64);
    let _writer = AbortOnDropHandle::new(tokio::spawn(write_client_frames(writer, out_rx)));
    let _stdin = stdin
        .map(|stdin| AbortOnDropHandle::new(tokio::spawn(forward_stdin(stdin, out_tx.clone()))));
    let read = read_until_terminal(
        reader,
        sink,
        out_tx.clone(),
        mcp,
        required,
        liveness.silence,
        |event| report_protocol(endpoint, event),
    );
    tokio::pin!(read);

    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = out_tx.send(ClientFrame::Cancel).await;
            let _ = tokio::time::timeout(liveness.cancel_grace, &mut read).await;
            Err(eyre::eyre!("secure command runner: the job was cancelled"))
        }
        finished = tokio::time::timeout(deadline, &mut read) => match finished {
            Ok(outcome) => outcome,
            Err(_) => Err(eyre::eyre!(
                "secure command runner unavailable: timed out waiting for completion"
            )),
        },
    };
    outcome
}

/// A durable job for [`dispatch_job`]: `exec` starts it if the worker does not
/// have it yet; without it the job is only attached to.
pub struct DurableJob {
    pub job: JobId,
    pub scope: ScopeId,
    pub exec: Option<ExecRequest>,
}

/// Run a durable job to its end, streaming its output to `sink` as plain
/// stdout and stderr frames. A lost link is met by attaching again from the
/// offsets already held, until `timeout` runs out; the job keeps running on
/// the worker meanwhile. Cancelling ends the job on the worker.
pub async fn dispatch_job(
    endpoint: &RunnerEndpoint,
    job: DurableJob,
    sink: mpsc::Sender<ServerFrame>,
    cancel: CancellationToken,
    mcp: Option<McpResponder>,
    timeout: Duration,
) -> Result<ExecSummary> {
    let deadline = tokio::time::Instant::now() + timeout;
    let busy_until = (tokio::time::Instant::now() + SCOPE_BUSY_WAIT).min(deadline);
    let held = Arc::new(parking_lot::Mutex::new(Offsets::default()));
    let mut pause = REATTACH_PAUSE_MIN;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let request = Request::Run(RunRequest {
            job: job.job.clone(),
            scope: job.scope.clone(),
            exec: job.exec.clone(),
            from: *held.lock(),
            timeout_secs: remaining.as_secs().max(1),
        });
        let (frames_tx, frames_rx) = mpsc::channel(64);
        let relay = tokio::spawn(relay_new_output(frames_rx, sink.clone(), Arc::clone(&held)));
        let attempt = dispatch(endpoint, request, frames_tx, cancel.clone(), mcp.clone()).await;
        let _ = relay.await;
        match attempt {
            Ok(Outcome::Done(summary)) => return Ok(summary),
            Ok(Outcome::Refused(RefusalReason::ScopeBusy))
                if tokio::time::Instant::now() + REATTACH_PAUSE_MIN < busy_until =>
            {
                tokio::select! {
                    _ = tokio::time::sleep(REATTACH_PAUSE_MIN) => {}
                    _ = cancel.cancelled() => {}
                }
            }
            Ok(Outcome::Refused(reason)) => {
                bail!("secure command runner refused the job: {reason}")
            }
            Ok(other) => bail!(
                "secure command runner answered a job with {}",
                other.describe()
            ),
            Err(e) if e.downcast_ref::<LinkLost>().is_none() || cancel.is_cancelled() => {
                return Err(e)
            }
            Err(e) if tokio::time::Instant::now() + pause >= deadline => {
                return Err(e.wrap_err("the job's time ran out before its worker could be reached"))
            }
            Err(e) => {
                tracing::warn!(job = %job.job, error = %e, "lost the link to a durable job; attaching again");
                tokio::select! {
                    _ = tokio::time::sleep(pause) => {}
                    _ = cancel.cancelled() => {}
                }
                pause = (pause * 2).min(REATTACH_PAUSE_MAX);
            }
        }
    }
}

/// Tell the worker it may drop a finished job's output. Best effort: a job
/// nobody forgets is dropped by the worker after a day.
pub async fn forget_job(endpoint: &RunnerEndpoint, job: &JobId) {
    let forgotten = dispatch(
        endpoint,
        Request::Forget { job: job.clone() },
        mpsc::channel(1).0,
        CancellationToken::new(),
        None,
    )
    .await;
    if let Err(e) = forgotten {
        tracing::debug!(job = %job, error = %e, "the worker keeps a finished job until it expires");
    }
}

/// End a job on the worker: attach to it and cancel at once, waiting the
/// cancel grace for it to stop. Best effort: a job that cannot be reached is
/// ended by its own deadline.
pub async fn cancel_job(endpoint: &RunnerEndpoint, job: &JobId, scope: &ScopeId) {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (sink, _output) = mpsc::channel(64);
    let request = Request::Run(RunRequest {
        job: job.clone(),
        scope: scope.clone(),
        exec: None,
        from: Offsets::default(),
        timeout_secs: CANCEL_TIMEOUT_SECS,
    });
    if let Err(e) = dispatch(endpoint, request, sink, cancel, None).await {
        tracing::debug!(job = %job, error = %e, "cancelled a job its round let go of");
    }
}

/// Forward the output this client does not hold yet as plain stdout and
/// stderr frames, and advance what it holds. The worker streams from the
/// offsets asked for, so a range that ends at or before them is a repeat.
async fn relay_new_output(
    mut frames: mpsc::Receiver<ServerFrame>,
    sink: mpsc::Sender<ServerFrame>,
    held: Arc<parking_lot::Mutex<Offsets>>,
) {
    while let Some(frame) = frames.recv().await {
        let ServerFrame::Output {
            stream, to, chunk, ..
        } = frame
        else {
            continue;
        };
        let fresh = {
            let mut held = held.lock();
            let known = match stream {
                Stream::Stdout => &mut held.stdout,
                Stream::Stderr => &mut held.stderr,
            };
            let fresh = to > *known;
            *known = (*known).max(to);
            fresh
        };
        if !fresh || chunk.is_empty() {
            continue;
        }
        let frame = match stream {
            Stream::Stdout => ServerFrame::Stdout { chunk },
            Stream::Stderr => ServerFrame::Stderr { chunk },
        };
        if sink.send(frame).await.is_err() {
            return;
        }
    }
}

async fn write_client_frames(
    mut writer: WriteHalf<Connection>,
    mut frames: mpsc::Receiver<ClientFrame>,
) {
    while let Some(frame) = frames.recv().await {
        if write_framed(&mut writer, &frame).await.is_err() {
            return;
        }
    }
}

async fn read_until_terminal(
    mut reader: ReadHalf<Connection>,
    sink: mpsc::Sender<ServerFrame>,
    out_tx: mpsc::Sender<ClientFrame>,
    mcp: Option<McpResponder>,
    required: u32,
    silence: Duration,
    report: impl Fn(ProtocolEvent),
) -> Result<Outcome> {
    let mut announced: Option<u32> = None;
    let mut answers = JoinSet::new();
    loop {
        let next = match silence_bound(announced, silence) {
            Some(bound) => tokio::time::timeout(bound, read_framed::<_, ServerFrame>(&mut reader))
                .await
                .map_err(|_| {
                    link_lost(format!(
                        "secure command runner unavailable: no heartbeat for {} seconds",
                        bound.as_secs()
                    ))
                })?,
            None => read_framed::<_, ServerFrame>(&mut reader).await,
        };
        match next {
            Ok(Some(ServerFrame::Ready { protocol_version })) => {
                if protocol_version < required {
                    report(ProtocolEvent::TooOld {
                        announced: Some(protocol_version),
                        required,
                    });
                    bail!(
                        "secure command runner is too old for this request: it announced \
                         protocol v{protocol_version}, which needs v{required}"
                    );
                }
                report(ProtocolEvent::Announced(protocol_version));
                announced = Some(protocol_version);
            }
            Ok(Some(ServerFrame::Heartbeat)) => {}
            Ok(Some(ServerFrame::McpRequest { id, body })) => {
                answers.spawn(answer_mcp(mcp.clone(), id, body, out_tx.clone()));
            }
            Ok(Some(
                frame @ (ServerFrame::Stdout { .. }
                | ServerFrame::Stderr { .. }
                | ServerFrame::Output { .. }),
            )) => {
                if sink.send(frame).await.is_err() {
                    bail!("secure command runner unavailable: output consumer dropped");
                }
            }
            Ok(Some(terminal)) => {
                if required > 0 && announced.is_none() {
                    report(ProtocolEvent::TooOld {
                        announced: None,
                        required,
                    });
                    bail!(
                        "secure command runner is too old for this request: it did not \
                         announce its protocol"
                    );
                }
                return Ok(match terminal {
                    ServerFrame::Done(summary) => Outcome::Done(summary),
                    ServerFrame::Prepared(prepared) => Outcome::Prepared(prepared),
                    ServerFrame::Healthy(health) => Outcome::Healthy(health),
                    ServerFrame::Refused { reason } => Outcome::Refused(reason),
                    ServerFrame::Failed { message } => {
                        bail!("secure command runner failed the request: {message}")
                    }
                    ServerFrame::Ready { .. }
                    | ServerFrame::Heartbeat
                    | ServerFrame::McpRequest { .. }
                    | ServerFrame::Stdout { .. }
                    | ServerFrame::Stderr { .. }
                    | ServerFrame::Output { .. } => unreachable!("handled above"),
                });
            }
            Ok(None) => {
                return Err(link_lost(
                    "secure command runner unavailable: connection closed before completion",
                ))
            }
            Err(e) => {
                return Err(link_lost(format!(
                    "secure command runner unavailable: stream error: {e}"
                )))
            }
        }
    }
}

/// A daemon below the heartbeat revision is silent while its job is, so only
/// the request deadline bounds it; every other peer must speak within `silence`.
fn silence_bound(announced: Option<u32>, silence: Duration) -> Option<Duration> {
    match announced {
        Some(version) if version < PROTOCOL_SCOPES => None,
        _ => Some(silence),
    }
}

async fn answer_mcp(
    mcp: Option<McpResponder>,
    id: u64,
    body: String,
    out_tx: mpsc::Sender<ClientFrame>,
) {
    let body = match mcp {
        Some(responder) => responder(body).await,
        None => unbound_reply(&body),
    };
    let _ = out_tx.send(ClientFrame::McpResponse { id, body }).await;
}

/// The JSON-RPC error a job receives when this client offers it no tools.
fn unbound_reply(request: &str) -> String {
    let id = serde_json::from_str::<serde_json::Value>(request)
        .ok()
        .and_then(|v| v.get("id").cloned())
        .unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32601, "message": "no MCP endpoint is bound to this job" },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> ExecRequest {
        ExecRequest {
            program: "echo".into(),
            args: vec!["hi".into()],
            workdir: None,
            timeout_secs: 5,
            allow_retry: false,
            env: Vec::new(),
            stdin: None,
            scope: None,
            mcp: None,
        }
    }

    /// Fail-closed: with no daemon at the socket, dispatch errors AND the
    /// command does not execute. We prove non-execution with a sentinel file.
    #[tokio::test]
    async fn no_server_fails_closed_without_executing() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = RunnerEndpoint::Unix(tmp.path().join("absent.sock"));
        let sentinel = tmp.path().join("sentinel.txt");

        let mut r = req();
        r.program = "sh".into();
        r.args = vec!["-c".into(), format!("touch {}", sentinel.display())];

        let (tx, _rx) = mpsc::channel(8);
        let err = dispatch_exec(&endpoint, r, tx, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("secure command runner unavailable"),
            "unexpected error: {err}"
        );
        assert!(!sentinel.exists(), "fail-closed violated: command executed");
    }

    /// A payload over the frame cap is refused before any connection is
    /// attempted, naming the cap, so the caller learns the real cause rather
    /// than a connection error.
    #[tokio::test]
    async fn an_oversized_request_is_refused_before_connecting() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = RunnerEndpoint::Unix(tmp.path().join("absent.sock"));
        let mut r = req();
        r.stdin = Some("x".repeat(super::super::protocol::MAX_FRAME_LEN));

        let (tx, _rx) = mpsc::channel(8);
        let err = dispatch_exec(&endpoint, r, tx, CancellationToken::new(), None)
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("exceeds"), "{text}");
        assert!(!text.contains("cannot connect"), "{text}");
    }

    #[test]
    fn an_unbound_reply_echoes_the_request_id() {
        let reply: serde_json::Value =
            serde_json::from_str(&unbound_reply(r#"{"jsonrpc":"2.0","id":9,"method":"x"}"#))
                .unwrap();
        assert_eq!(reply["id"], 9);
        assert_eq!(reply["error"]["code"], -32601);
        let fallback: serde_json::Value = serde_json::from_str(&unbound_reply("not json")).unwrap();
        assert!(fallback["id"].is_null());
    }
}

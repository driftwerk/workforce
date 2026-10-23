//! JSON-RPC with a language server over its stdio: `Content-Length` framed
//! messages, requests matched to replies, the server's own requests
//! answered, and what it publishes (diagnostics, whether it is busy) kept
//! for whoever waits on it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::debug;

/// Largest message read from a server; past it the connection is dropped.
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// How long a message may wait for room in the server's input queue.
const SEND_WAIT: Duration = Duration::from_secs(30);

fn frame(message: &Value) -> String {
    let body = message.to_string();
    format!("Content-Length: {}\r\n\r\n{body}", body.len())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    /// The server is gone.
    Closed,
    /// It did not answer in time; it may be wedged.
    TimedOut,
    /// It answered with an error.
    Server(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => f.write_str("the language server exited"),
            Self::TimedOut => f.write_str("the language server did not answer in time"),
            Self::Server(message) => f.write_str(message),
        }
    }
}

/// Diagnostics a server published for one document.
#[derive(Debug, Clone, PartialEq)]
pub struct Published {
    /// Order among everything the server published.
    pub seq: u64,
    pub version: Option<i64>,
    pub diagnostics: Vec<Value>,
}

#[derive(Default)]
struct Shared {
    by_uri: Mutex<HashMap<String, Published>>,
    quiescent: Mutex<Option<bool>>,
    seq: AtomicU64,
    changed: Notify,
}

type Reply = Result<Value, RpcError>;

pub struct Rpc {
    out: mpsc::Sender<String>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Reply>>>>,
    next: AtomicI64,
    state: Arc<Shared>,
    closed: CancellationToken,
    _reader: AbortOnDropHandle<()>,
}

impl Rpc {
    /// Speak to a server that reads `out` and writes `input`. `folders` is
    /// what it is told when it asks for the workspace folders.
    pub fn new(out: mpsc::Sender<String>, input: mpsc::Receiver<String>, folders: Value) -> Self {
        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Reply>>>> = Arc::default();
        let state: Arc<Shared> = Arc::default();
        let closed = CancellationToken::new();
        let reader = tokio::spawn(read(
            input,
            out.clone(),
            Arc::clone(&pending),
            Arc::clone(&state),
            closed.clone(),
            folders,
        ));
        Self {
            out,
            pending,
            next: AtomicI64::new(1),
            state,
            closed,
            _reader: AbortOnDropHandle::new(reader),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    /// Resolves once the server is gone.
    pub async fn closed(&self) {
        self.closed.cancelled().await
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// Ask `method` and wait up to `timeout` for the answer. Dropped before
    /// the answer, the request is cancelled on the server.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        let mut outstanding = Outstanding { rpc: self, id };
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let reply = tokio::select! {
            biased;
            reply = tokio::time::timeout(timeout, rx) => match reply {
                Ok(Ok(reply)) => reply,
                Ok(Err(_)) => Err(RpcError::Closed),
                Err(_) => Err(RpcError::TimedOut),
            },
            _ = self.closed.cancelled() => Err(RpcError::Closed),
        };
        if reply.is_ok() || reply == Err(RpcError::Closed) {
            outstanding.id = 0;
        }
        reply
    }

    /// Queue `message` for the server. A server that has stopped reading
    /// leaves the queue full, which is a wedged server, never a wait.
    async fn send(&self, message: &Value) -> Result<(), RpcError> {
        match tokio::time::timeout(SEND_WAIT, self.out.send(frame(message))).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(RpcError::Closed),
            Err(_) => Err(RpcError::TimedOut),
        }
    }

    /// Drop what the server published for a document that is closed.
    pub fn forget_diagnostics(&self, uri: &str) {
        self.state.by_uri.lock().remove(uri);
    }

    /// The diagnostics last published for `uri` after `after`, once the
    /// server has gone `settle` without publishing it again; `None` when it
    /// published none within `wait`.
    pub async fn diagnostics_after(
        &self,
        uri: &str,
        after: u64,
        wait: Duration,
        settle: Duration,
    ) -> Option<Published> {
        let latest = || {
            self.state
                .by_uri
                .lock()
                .get(uri)
                .filter(|published| published.seq > after)
                .cloned()
        };
        let deadline = tokio::time::Instant::now() + wait;
        let mut seen: Option<Published> = None;
        loop {
            let changed = self.state.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(now) = latest() {
                if seen.as_ref().is_some_and(|seen| seen.seq == now.seq) {
                    return seen;
                }
                seen = Some(now);
                let quiet = tokio::time::timeout(settle, &mut changed).await.is_err();
                if quiet || tokio::time::Instant::now() >= deadline {
                    return latest().or(seen);
                }
                continue;
            }
            if tokio::time::timeout_at(deadline, &mut changed)
                .await
                .is_err()
                || self.is_closed()
            {
                return latest();
            }
        }
    }

    /// The order of the last publication, for waiting on the next one.
    pub fn published_seq(&self) -> u64 {
        self.state.seq.load(Ordering::SeqCst)
    }

    /// Wait until the server reports it is done loading and analysing, as
    /// rust-analyzer's `experimental/serverStatus` does; `false` when it was
    /// still busy at `deadline` or went away.
    pub async fn quiescent_by(&self, deadline: tokio::time::Instant) -> bool {
        loop {
            let changed = self.state.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if *self.state.quiescent.lock() == Some(true) {
                return true;
            }
            if self.is_closed() {
                return false;
            }
            tokio::select! {
                timed_out = tokio::time::timeout_at(deadline, &mut changed) => {
                    if timed_out.is_err() {
                        return false;
                    }
                }
                _ = self.closed.cancelled() => return false,
            }
        }
    }
}

/// A request not yet answered: dropping it forgets it here and cancels it
/// on the server.
struct Outstanding<'a> {
    rpc: &'a Rpc,
    id: i64,
}

impl Drop for Outstanding<'_> {
    fn drop(&mut self) {
        if self.id == 0 {
            return;
        }
        self.rpc.pending.lock().remove(&self.id);
        let cancel =
            json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": self.id}});
        let _ = self.rpc.out.try_send(frame(&cancel));
    }
}

async fn read(
    mut input: mpsc::Receiver<String>,
    out: mpsc::Sender<String>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Reply>>>>,
    state: Arc<Shared>,
    closed: CancellationToken,
    folders: Value,
) {
    let mut buffer: Vec<u8> = Vec::new();
    'stream: while let Some(chunk) = input.recv().await {
        buffer.extend_from_slice(chunk.as_bytes());
        loop {
            match take_message(&mut buffer) {
                Ok(Some(message)) => {
                    if let Some(reply) = handle(&message, &pending, &state, &folders) {
                        let sent = tokio::time::timeout(SEND_WAIT, out.send(frame(&reply))).await;
                        if !matches!(sent, Ok(Ok(()))) {
                            break 'stream;
                        }
                    }
                }
                Ok(None) => break,
                Err(reason) => {
                    debug!(%reason, "unreadable message from a language server; dropping it");
                    break 'stream;
                }
            }
        }
    }
    closed.cancel();
    pending.lock().clear();
    state.changed.notify_waiters();
}

/// The next whole message in `buffer`, removed from it.
pub(super) fn take_message(buffer: &mut Vec<u8>) -> Result<Option<Value>, String> {
    let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
        return match buffer.len() > 8 * 1024 {
            true => Err("a header without an end".into()),
            false => Ok(None),
        };
    };
    let headers = std::str::from_utf8(&buffer[..end]).map_err(|e| e.to_string())?;
    let length: usize = headers
        .split("\r\n")
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .ok_or("a message without a length")?;
    if length > MAX_MESSAGE {
        return Err(format!("a {length}-byte message"));
    }
    let start = end + 4;
    if buffer.len() < start + length {
        return Ok(None);
    }
    let message = serde_json::from_slice(&buffer[start..start + length]).map_err(|e| e.to_string());
    buffer.drain(..start + length);
    message.map(Some)
}

/// Act on one message; what to answer, for a request from the server.
fn handle(
    message: &Value,
    pending: &Mutex<HashMap<i64, oneshot::Sender<Reply>>>,
    state: &Shared,
    folders: &Value,
) -> Option<Value> {
    let method = message.get("method").and_then(Value::as_str);
    let id = message.get("id");
    match (method, id) {
        (None, Some(id)) => {
            let waiter = id.as_i64().and_then(|id| pending.lock().remove(&id));
            if let Some(waiter) = waiter {
                let reply = match message.get("error") {
                    Some(error) => Err(RpcError::Server(
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("the language server refused the request")
                            .to_string(),
                    )),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = waiter.send(reply);
            }
            None
        }
        (Some(method), Some(id)) => {
            let params = message.get("params").unwrap_or(&Value::Null);
            let answer = match method {
                "workspace/configuration" => Ok(Value::Array(
                    params
                        .get("items")
                        .and_then(Value::as_array)
                        .map(|items| items.iter().map(|_| Value::Null).collect())
                        .unwrap_or_default(),
                )),
                "workspace/workspaceFolders" => Ok(folders.clone()),
                "workspace/applyEdit" => Ok(json!({
                    "applied": false,
                    "failureReason": "edits are applied by the client, not on the server's request"
                })),
                "window/workDoneProgress/create"
                | "client/registerCapability"
                | "client/unregisterCapability"
                | "window/showMessageRequest"
                | "workspace/diagnostic/refresh"
                | "workspace/semanticTokens/refresh"
                | "workspace/inlayHint/refresh"
                | "workspace/codeLens/refresh" => Ok(Value::Null),
                _ => Err(json!({"code": -32601, "message": format!("{method} is not supported")})),
            };
            Some(match answer {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
            })
        }
        (Some("textDocument/publishDiagnostics"), None) => {
            let params = message.get("params")?;
            let uri = params.get("uri")?.as_str()?.to_string();
            let published = Published {
                seq: state.seq.fetch_add(1, Ordering::SeqCst) + 1,
                version: params.get("version").and_then(Value::as_i64),
                diagnostics: params
                    .get("diagnostics")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            };
            state.by_uri.lock().insert(uri, published);
            state.changed.notify_waiters();
            None
        }
        (Some("experimental/serverStatus"), None) => {
            let quiescent = message
                .pointer("/params/quiescent")
                .and_then(Value::as_bool);
            *state.quiescent.lock() = quiescent;
            state.changed.notify_waiters();
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn next_message(buffer: &mut Vec<u8>, input: &mut mpsc::Receiver<String>) -> Value {
        loop {
            if let Some(message) = take_message(buffer).unwrap() {
                return message;
            }
            buffer.extend_from_slice(input.recv().await.unwrap().as_bytes());
        }
    }

    #[test]
    fn messages_are_cut_by_their_byte_length_across_chunks() {
        let message = json!({"jsonrpc": "2.0", "method": "x", "params": {"text": "é—✓"}});
        let framed = frame(&message);
        let mut buffer = Vec::new();
        let (head, tail) = framed.as_bytes().split_at(framed.len() - 3);
        buffer.extend_from_slice(head);
        assert_eq!(take_message(&mut buffer), Ok(None));
        buffer.extend_from_slice(tail);
        buffer.extend_from_slice(frame(&json!({"id": 2})).as_bytes());
        assert_eq!(take_message(&mut buffer), Ok(Some(message)));
        assert_eq!(take_message(&mut buffer), Ok(Some(json!({"id": 2}))));
        assert!(buffer.is_empty());
    }

    #[tokio::test]
    async fn a_request_gets_its_own_reply_and_the_servers_requests_are_answered() {
        let (to_server, mut server_reads) = mpsc::channel::<String>(16);
        let (server_writes, from_server) = mpsc::channel::<String>(16);
        let rpc = Rpc::new(
            to_server,
            from_server,
            json!([{"uri": "file:///w", "name": "w"}]),
        );
        let server = tokio::spawn(async move {
            let mut buffer = Vec::new();
            let request = next_message(&mut buffer, &mut server_reads).await;
            server_writes
                .send(frame(&json!({"jsonrpc": "2.0", "id": 99, "method": "workspace/applyEdit", "params": {}})))
                .await
                .unwrap();
            let answer = next_message(&mut buffer, &mut server_reads).await;
            server_writes
                .send(frame(
                    &json!({"jsonrpc": "2.0", "id": request["id"], "result": {"ok": true}}),
                ))
                .await
                .unwrap();
            answer
        });
        let reply = rpc
            .request("textDocument/rename", json!({}), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(reply, json!({"ok": true}));
        let answer = server.await.unwrap();
        assert_eq!(answer["id"], 99);
        assert_eq!(
            answer["result"]["applied"], false,
            "the server never edits files itself"
        );
    }

    #[tokio::test]
    async fn a_request_to_a_server_that_went_away_fails_as_closed() {
        let (to_server, _server_reads) = mpsc::channel::<String>(16);
        let (server_writes, from_server) = mpsc::channel::<String>(16);
        let rpc = Rpc::new(to_server, from_server, Value::Null);
        drop(server_writes);
        let reply = rpc
            .request("initialize", json!({}), Duration::from_secs(5))
            .await;
        assert_eq!(reply, Err(RpcError::Closed));
        assert!(rpc.is_closed());
    }

    #[tokio::test]
    async fn diagnostics_are_the_last_published_once_the_server_settles() {
        let (to_server, _server_reads) = mpsc::channel::<String>(16);
        let (server_writes, from_server) = mpsc::channel::<String>(16);
        let rpc = Rpc::new(to_server, from_server, Value::Null);
        let before = rpc.published_seq();
        let publish = |count: usize| {
            frame(
                &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
                    "uri": "file:///a.ts", "version": 1,
                    "diagnostics": vec![json!({"message": "m"}); count],
                }}),
            )
        };
        server_writes.send(publish(1)).await.unwrap();
        server_writes.send(publish(2)).await.unwrap();
        let published = rpc
            .diagnostics_after(
                "file:///a.ts",
                before,
                Duration::from_secs(5),
                Duration::from_millis(200),
            )
            .await
            .unwrap();
        assert_eq!(published.diagnostics.len(), 2);
        assert_eq!(published.version, Some(1));
        let none = rpc
            .diagnostics_after(
                "file:///b.ts",
                before,
                Duration::from_millis(100),
                Duration::from_millis(50),
            )
            .await;
        assert_eq!(none, None);

        rpc.forget_diagnostics("file:///a.ts");
        let forgotten = rpc
            .diagnostics_after(
                "file:///a.ts",
                before,
                Duration::from_millis(100),
                Duration::from_millis(50),
            )
            .await;
        assert_eq!(
            forgotten, None,
            "a closed document's diagnostics are dropped"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_stops_reading_times_a_message_out_instead_of_holding_the_caller() {
        let (to_server, _never_read) = mpsc::channel::<String>(1);
        let (_server_writes, from_server) = mpsc::channel::<String>(16);
        let rpc = Rpc::new(to_server, from_server, Value::Null);
        rpc.notify("textDocument/didOpen", json!({})).await.unwrap();
        assert_eq!(
            rpc.notify("textDocument/didOpen", json!({})).await,
            Err(RpcError::TimedOut)
        );
        assert_eq!(
            rpc.request("textDocument/rename", json!({}), Duration::from_secs(1))
                .await,
            Err(RpcError::TimedOut)
        );
    }
}

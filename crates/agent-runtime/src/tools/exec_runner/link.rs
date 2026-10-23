//! A running job's handle on its connection: where its output goes, how it
//! learns it was cancelled or lost its client, and how it calls back into
//! the client.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use eyre::{bail, Result};
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio_util::sync::CancellationToken;

use super::protocol::ServerFrame;

/// Tool calls a job may have outstanding at once. The bridge applies the
/// bound as backpressure rather than refusing the next call.
pub const MCP_IN_FLIGHT: usize = 16;

/// How long a durable job's tool call waits for a client to attach: long
/// enough for a hub to drain, a successor to take the turn up, and a daemon
/// to restart. A client serving the job's calls must wait at least this much
/// longer than the call itself.
pub const ATTACH_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

pub struct JobLink {
    frames: mpsc::Sender<ServerFrame>,
    bridge: Arc<McpBridge>,
    asked: CancellationToken,
    gone: CancellationToken,
    stdin: Mutex<Option<mpsc::Receiver<String>>>,
}

impl JobLink {
    pub(super) fn new(
        frames: mpsc::Sender<ServerFrame>,
        asked: CancellationToken,
        gone: CancellationToken,
    ) -> Self {
        Self {
            frames,
            bridge: Arc::new(McpBridge::default()),
            asked,
            gone,
            stdin: Mutex::new(None),
        }
    }

    /// The link of an interactive job, whose stdin the client feeds.
    pub(super) fn with_stdin(self, stdin: mpsc::Receiver<String>) -> Self {
        *self.stdin.lock() = Some(stdin);
        self
    }

    /// What the client writes to the job's stdin, for the one taker; `None`
    /// for a job that is not interactive.
    pub fn take_stdin(&self) -> Option<mpsc::Receiver<String>> {
        self.stdin.lock().take()
    }

    pub(super) fn bridge(&self) -> Arc<McpBridge> {
        Arc::clone(&self.bridge)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(frames: mpsc::Sender<ServerFrame>) -> Self {
        Self::new(frames, CancellationToken::new(), CancellationToken::new())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests_with_cancel(
        frames: mpsc::Sender<ServerFrame>,
        cancel: CancellationToken,
    ) -> Self {
        Self::new(frames, cancel, CancellationToken::new())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests_with_tokens(
        frames: mpsc::Sender<ServerFrame>,
        asked: CancellationToken,
        gone: CancellationToken,
    ) -> Self {
        Self::new(frames, asked, gone)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn fulfil_for_tests(&self, id: u64, body: String) {
        self.bridge.fulfil(id, body)
    }

    /// A sender for streamed output, for code that already writes frames to
    /// a channel.
    pub fn sink(&self) -> mpsc::Sender<ServerFrame> {
        self.frames.clone()
    }

    pub async fn emit(&self, frame: ServerFrame) -> Result<()> {
        if self.frames.send(frame).await.is_err() {
            bail!("the client went away");
        }
        Ok(())
    }

    /// Resolves when the client asks for the job to end or goes away: the
    /// end of a job that lives as long as its connection.
    pub async fn cancelled(&self) {
        tokio::select! {
            _ = self.asked.cancelled() => {}
            _ = self.gone.cancelled() => {}
        }
    }

    /// Resolves only when the client asks for the job to end. A job that
    /// outlives its connection waits on this, not on [`Self::cancelled`].
    pub async fn cancel_requested(&self) {
        self.asked.cancelled().await
    }

    /// Send one MCP request to the client and wait for its reply.
    pub async fn mcp_call(&self, body: String) -> Result<String> {
        let _permit = self.bridge.permits.acquire().await?;
        let (id, reply) = self.bridge.register();
        self.emit(ServerFrame::McpRequest { id, body }).await?;
        tokio::select! {
            answer = reply => answer.map_err(|_| eyre::eyre!("the connection closed before the tool call was answered")),
            _ = self.cancelled() => {
                self.bridge.forget(id);
                bail!("cancelled before the tool call was answered")
            }
        }
    }
}

pub(super) struct McpBridge {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<String>>>,
    permits: Semaphore,
}

impl Default for McpBridge {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            permits: Semaphore::new(MCP_IN_FLIGHT),
        }
    }
}

impl McpBridge {
    fn register(&self) -> (u64, oneshot::Receiver<String>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        (id, rx)
    }

    fn forget(&self, id: u64) {
        self.pending.lock().remove(&id);
    }

    /// Deliver the client's reply; a reply for an unknown or already
    /// abandoned call is dropped.
    pub(super) fn fulfil(&self, id: u64, body: String) {
        if let Some(tx) = self.pending.lock().remove(&id) {
            let _ = tx.send(body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_call_is_answered_by_the_reply_carrying_its_id() {
        let (tx, mut rx) = mpsc::channel(8);
        let link = JobLink::for_tests(tx);
        let bridge = link.bridge();
        let call = tokio::spawn(async move { link.mcp_call("ping".into()).await });
        let ServerFrame::McpRequest { id, body } = rx.recv().await.unwrap() else {
            panic!("expected an MCP request frame")
        };
        assert_eq!(body, "ping");
        bridge.fulfil(id + 1, "wrong".into());
        bridge.fulfil(id, "pong".into());
        assert_eq!(call.await.unwrap().unwrap(), "pong");
    }

    #[tokio::test]
    async fn cancellation_releases_a_waiting_call() {
        let (tx, _rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let link = JobLink::for_tests_with_cancel(tx, cancel.clone());
        let call = tokio::spawn(async move { link.mcp_call("ping".into()).await });
        tokio::task::yield_now().await;
        cancel.cancel();
        let err = call.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
    }

    #[tokio::test]
    async fn only_an_explicit_cancel_is_a_request_to_end_the_job() {
        let (tx, _rx) = mpsc::channel(8);
        let (asked, gone) = (CancellationToken::new(), CancellationToken::new());
        let link = JobLink::for_tests_with_tokens(tx, asked.clone(), gone.clone());
        gone.cancel();
        link.cancelled().await;
        let requested = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            link.cancel_requested(),
        );
        assert!(requested.await.is_err(), "a lost client is not a cancel");
        asked.cancel();
        link.cancel_requested().await;
    }
}

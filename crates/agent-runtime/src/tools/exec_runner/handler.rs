//! What a daemon does with a request once the connection is authenticated.
//! The transport and framing are shared; only this differs between the
//! hub-local runner and a worker.

use async_trait::async_trait;
use eyre::Result;

use super::link::JobLink;
use super::protocol::{ExecSummary, RefusalReason, Request, ServerFrame, WorkerHealth};
use super::run_exec::{run_exec, run_interactive, EnvMode};

#[async_trait]
pub trait JobHandler: Send + Sync {
    /// Run one request to its terminal frame, streaming output through
    /// `link` and honouring its cancellation.
    async fn run(&self, request: Request, link: JobLink) -> Result<ServerFrame>;
}

/// The daemon on the hub image: it shares the hub's filesystem, so it serves
/// plain commands with the scrubbed environment and has no scopes to offer.
pub struct LocalJobHandler;

#[async_trait]
impl JobHandler for LocalJobHandler {
    async fn run(&self, request: Request, link: JobLink) -> Result<ServerFrame> {
        match request {
            Request::Exec(req) if req.scope.is_none() && req.mcp.is_none() => {
                let exec = run_exec(&req, &[], EnvMode::Scrubbed, link.sink());
                tokio::pin!(exec);
                tokio::select! {
                    biased;
                    _ = link.cancelled() => Ok(ServerFrame::Done(ExecSummary::killed())),
                    summary = &mut exec => Ok(ServerFrame::Done(summary?)),
                }
            }
            Request::Interactive(req) if req.exec.scope.is_none() && req.exec.mcp.is_none() => {
                let stdin = link
                    .take_stdin()
                    .unwrap_or_else(|| tokio::sync::mpsc::channel(1).1);
                let interactive = run_interactive(&req, &[], EnvMode::Scrubbed, link.sink(), stdin);
                tokio::pin!(interactive);
                tokio::select! {
                    biased;
                    _ = link.cancelled() => Ok(ServerFrame::Done(ExecSummary::killed())),
                    summary = &mut interactive => Ok(ServerFrame::Done(summary?)),
                }
            }
            Request::Exec(_)
            | Request::Interactive(_)
            | Request::Prepare(_)
            | Request::Release { .. }
            | Request::Run(_)
            | Request::Forget { .. } => Ok(ServerFrame::Refused {
                reason: RefusalReason::ProtocolMismatch,
            }),
            Request::Health => Ok(ServerFrame::Healthy(WorkerHealth::without_scopes())),
        }
    }
}

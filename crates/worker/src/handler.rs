//! The daemon's answer to each request kind, over the shared scope engine.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use workforce_agent::tools::exec_runner::{
    JobHandler, JobLink, RefusalReason, Request, ServerFrame,
};

use crate::scopes::Worker;

pub struct WorkerHandler {
    worker: Arc<Worker>,
}

impl WorkerHandler {
    pub fn new(worker: Arc<Worker>) -> Self {
        Self { worker }
    }
}

#[async_trait]
impl JobHandler for WorkerHandler {
    async fn run(&self, request: Request, link: JobLink) -> Result<ServerFrame> {
        match request {
            Request::Exec(exec) => self.worker.exec(exec, Arc::new(link)).await,
            Request::Prepare(prepare) => self.worker.prepare(prepare).await,
            Request::Release { scope } => self.worker.release(&scope).await,
            Request::Health => Ok(ServerFrame::Healthy(self.worker.health().await?)),
            Request::Run(run) => self.worker.run(run, Arc::new(link)).await,
            Request::Forget { job } => self.worker.forget(&job),
            Request::Interactive(_) => Ok(ServerFrame::Refused {
                reason: RefusalReason::ProtocolMismatch,
            }),
        }
    }
}

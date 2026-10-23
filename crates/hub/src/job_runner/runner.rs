//! Core job runner — manages background job execution with concurrency control.
//!
//! The `JobRunner` spawns tokio tasks for each job, enforces a concurrent
//! job limit with FIFO queueing, persists manifests to disk, and publishes
//! completion notifications via JetStream and/or channel surfaces.
//!
//! Extended with chain execution, retry scheduling, and output streaming.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock};

use chrono::Utc;
use eyre::{bail, Result};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use workforce_agent::tools::ToolResult;
use workforce_nats::client::{publish_json, JetstreamPublisher};

use super::chain::{self, get_all_downstream, get_ready_steps, ChainSummary};
use super::monitor::{parse_condition, resolve_path};
use super::persistence::JobPersistence;
use super::retry;
use super::streaming::OutputStreamer;
use super::types::*;
pub use crate::hub_core::jobs::AgentExecutor;
pub use crate::hub_core::jobs::AgentRunResult;
pub use crate::hub_core::jobs::JobRunnerCacheStats;

/// Handle to a running job's tokio task.
struct RunningJob {
    #[allow(dead_code)]
    handle: JoinHandle<()>,
    /// Cancellation token.
    cancel: tokio::sync::watch::Sender<bool>,
    /// Output streamer handle (for toggling streaming on running jobs).
    #[allow(dead_code)]
    streamer: Option<OutputStreamer>,
}

/// Trait for executing tools by name — avoids circular dependency on ToolRegistry.
#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Execute a tool by name with the given parameters.
    async fn execute_tool(&self, tool_name: &str, params: serde_json::Value) -> Result<ToolResult>;

    /// Check if a tool exists.
    fn has_tool(&self, tool_name: &str) -> bool;

    /// Why calling `tool_name` with `params` as `user` is refused, if it is.
    /// A job is checked when it is submitted as well as when it runs, so the
    /// turn submitting it hears the refusal instead of finding a failed job.
    fn refusal(
        &self,
        _user: Option<&str>,
        _tool_name: &str,
        _params: &serde_json::Value,
    ) -> Option<String> {
        None
    }
}

/// Trait for delivering job completion notifications to channels.
///
/// Allows the runner to announce results without depending on a specific
/// messaging implementation.
#[async_trait::async_trait]
pub trait NotificationSink: Send + Sync {
    /// Send a notification message to a channel/thread.
    async fn send_notification(
        &self,
        channel: &str,
        thread: Option<&str>,
        text: &str,
    ) -> Result<()>;
}

/// Trait for recording job execution metrics into the stats/telemetry system.
///
/// Decouples the job runner from the concrete `CostTracker`/`SqliteStore`
/// implementations so the hub can wire its own telemetry backend.
#[async_trait::async_trait]
pub trait StatsRecorder: Send + Sync {
    /// Record a completed job's telemetry (duration, type, success/failure).
    async fn record_job_telemetry(&self, manifest: &JobManifest, output: Option<&JobOutput>);
}

/// Trait for injecting job completion notifications into active agent sessions.
///
/// When a job completes, the runner can push a system message into the
/// originating session so the agent sees the result without polling.
#[async_trait::async_trait]
pub trait SessionInjector: Send + Sync {
    /// Inject a job completion message into the session that submitted the job.
    /// Returns `true` if the session was found and the message was injected.
    async fn inject_job_completion(&self, session_channel: &str, message: &str) -> Result<bool>;
}

/// Inner state of the job runner — held behind Arc for sharing with tasks.
struct Inner {
    agent_id: String,
    jobs: RwLock<HashMap<JobId, JobManifest>>,
    handles: Mutex<HashMap<JobId, RunningJob>>,
    queue: Mutex<VecDeque<JobManifest>>,
    semaphore: Arc<Semaphore>,
    persistence: JobPersistence,
    publisher: Option<Arc<dyn JetstreamPublisher>>,
    notification_sink: Option<Arc<dyn NotificationSink>>,
    stats_recorder: Option<Arc<dyn StatsRecorder>>,
    session_injector: Option<Arc<dyn SessionInjector>>,
    tool_executor: Arc<dyn ToolExecutor>,
    /// Executor for agent jobs, wired after construction (see
    /// [`JobRunner::set_agent_executor`]).
    agent_executor: OnceLock<Arc<dyn AgentExecutor>>,
    /// Channel for signalling that a job slot has freed up.
    dequeue_tx: tokio::sync::mpsc::Sender<()>,
    /// Active chains.
    chains: RwLock<HashMap<ChainId, JobChain>>,
    /// Active streamers for running jobs (for toggling).
    streamers: RwLock<HashMap<JobId, OutputStreamer>>,
}

/// The core job runner. Cheaply cloneable via internal Arc.
#[derive(Clone)]
pub struct JobRunner {
    inner: Arc<Inner>,
}

/// Whether `manifest` is an agent job run as or billed to anyone in `names`.
fn agent_job_names(manifest: &JobManifest, names: &[String]) -> bool {
    match &manifest.config {
        JobConfig::Agent(job) => [&job.user_id, &job.bill_to]
            .into_iter()
            .any(|slot| slot.as_ref().is_some_and(|id| names.contains(id))),
        _ => false,
    }
}

impl JobRunner {
    /// Create a new job runner.
    ///
    /// `publisher` is optional — if `None`, JetStream notifications are skipped.
    /// `notification_sink` is optional — if set, job completions are announced
    /// to the originating channel/thread.
    /// `stats_recorder` is optional — if set, job metrics are recorded on completion.
    /// `session_injector` is optional — if set, completion messages are injected
    /// into the originating agent session.
    pub fn new(
        agent_id: String,
        persistence: JobPersistence,
        publisher: Option<Arc<dyn JetstreamPublisher>>,
        notification_sink: Option<Arc<dyn NotificationSink>>,
        stats_recorder: Option<Arc<dyn StatsRecorder>>,
        session_injector: Option<Arc<dyn SessionInjector>>,
        tool_executor: Arc<dyn ToolExecutor>,
        concurrent_limit: usize,
    ) -> Self {
        let (dequeue_tx, dequeue_rx) = tokio::sync::mpsc::channel(concurrent_limit);
        let runner = Self {
            inner: Arc::new(Inner {
                agent_id,
                jobs: RwLock::new(HashMap::new()),
                handles: Mutex::new(HashMap::new()),
                queue: Mutex::new(VecDeque::new()),
                semaphore: Arc::new(Semaphore::new(concurrent_limit)),
                persistence,
                publisher,
                notification_sink,
                stats_recorder,
                session_injector,
                tool_executor,
                agent_executor: OnceLock::new(),
                dequeue_tx,
                chains: RwLock::new(HashMap::new()),
                streamers: RwLock::new(HashMap::new()),
            }),
        };

        // Spawn a background task that processes dequeue signals.
        let runner_clone = runner.clone();
        tokio::spawn(async move {
            runner_clone.dequeue_loop(dequeue_rx).await;
        });

        runner
    }

    /// Wire the agent-job executor after construction. The first call wins;
    /// later calls are ignored.
    pub fn set_agent_executor(&self, executor: Arc<dyn AgentExecutor>) {
        let _ = self.inner.agent_executor.set(executor);
    }

    /// Snapshot the in-memory job-runner maps for memory observability.
    /// Each field reflects an unbounded `RwLock<HashMap>` / `Mutex<VecDeque>`;
    /// emit-and-watch lets us see whether retention work is actually
    /// trimming them.
    pub async fn cache_stats(&self) -> JobRunnerCacheStats {
        let jobs = self.inner.jobs.read().await.len() as u64;
        let handles = self.inner.handles.lock().await.len() as u64;
        let queue = self.inner.queue.lock().await.len() as u64;
        let chains = self.inner.chains.read().await.len() as u64;
        let streamers = self.inner.streamers.read().await.len() as u64;
        JobRunnerCacheStats {
            jobs,
            handles,
            queue,
            chains,
            streamers,
        }
    }

    /// Background loop: when a job completes and frees a slot, try to start queued jobs.
    async fn dequeue_loop(&self, mut rx: tokio::sync::mpsc::Receiver<()>) {
        while rx.recv().await.is_some() {
            let next = {
                let mut q = self.inner.queue.lock().await;
                q.pop_front()
            };
            if let Some(manifest) = next {
                if let Err(e) = self.try_start_or_queue(manifest).await {
                    error!(error = %e, "Failed to start queued job");
                }
            }
        }
    }

    /// Perform startup recovery and resume monitor/retrying jobs.
    pub async fn recover(&self) -> Result<()> {
        self.inner.persistence.ensure_dir().await?;
        let result = super::recovery::recover_on_startup(&self.inner.persistence).await?;

        // Resume monitor jobs.
        for manifest in result.monitors_to_resume {
            let job_id = manifest.job_id.clone();
            {
                let mut jobs = self.inner.jobs.write().await;
                jobs.insert(job_id.clone(), manifest.clone());
            }
            if let Err(e) = self.try_start_or_queue(manifest).await {
                error!(job_id = %job_id, error = %e, "Failed to resume monitor job");
            }
        }

        // Resume retrying jobs.
        for manifest in result.retries_to_resume {
            let job_id = manifest.job_id.clone();
            {
                let mut jobs = self.inner.jobs.write().await;
                jobs.insert(job_id.clone(), manifest.clone());
            }
            self.schedule_retry(manifest);
        }

        // Handle chain failures — cascade cancellations.
        for chain_recovery in result.chains_with_failures {
            let mut chain = chain_recovery.chain;
            for failed_step in &chain_recovery.failed_steps {
                self.cascade_chain_failure(&mut chain, failed_step).await;
            }
            chain.status = chain.compute_status();
            if chain.status.is_terminal() {
                chain.completed_at = Some(Utc::now());
            }
            let _ = self.inner.persistence.save_chain(&chain).await;
            {
                let mut chains = self.inner.chains.write().await;
                chains.insert(chain.id.clone(), chain);
            }
        }

        // Load non-terminal chains into memory.
        if let Ok(all_chains) = self.inner.persistence.scan_all_chains().await {
            let mut chains = self.inner.chains.write().await;
            for chain in all_chains {
                if !chain.status.is_terminal() && !chains.contains_key(&chain.id) {
                    chains.insert(chain.id.clone(), chain);
                }
            }
        }

        // Clean up old completed jobs (keep last 100).
        if let Err(e) = self.inner.persistence.cleanup_old_jobs(100).await {
            warn!(error = %e, "Failed to clean up old jobs");
        }

        Ok(())
    }

    /// Why the tool call a job of `config` makes would be refused when it runs
    /// as `principal`. An agent job's turn meets the gates itself.
    fn refusal(
        &self,
        config: &JobConfig,
        principal: &workforce_common::session::TurnPrincipal,
        timeout_secs: u64,
    ) -> Option<String> {
        let (tool, params) = match config {
            JobConfig::Tool(t) => (t.tool.as_str(), t.params.clone()),
            JobConfig::Monitor(m) => (m.tool.as_str(), m.params.clone()),
            JobConfig::Shell(s) => (
                "exec.run",
                shell_params(&s.command, s.workdir.as_deref(), timeout_secs),
            ),
            JobConfig::Agent(_) => return None,
        };
        self.inner
            .tool_executor
            .refusal(principal.run_as.as_deref(), tool, &params)
    }

    /// Submit a new job. Returns the job ID immediately.
    pub async fn submit(&self, manifest: JobManifest) -> Result<JobId> {
        let job_id = manifest.job_id.clone();

        // Validate tool exists for tool/monitor jobs.
        match &manifest.config {
            JobConfig::Tool(t) => {
                if !self.inner.tool_executor.has_tool(&t.tool) {
                    bail!("Tool '{}' not found in registry", t.tool);
                }
            }
            JobConfig::Monitor(m) => {
                if !self.inner.tool_executor.has_tool(&m.tool) {
                    bail!("Tool '{}' not found in registry", m.tool);
                }
                // A condition monitor must have a valid expression; a change
                // watch needs none. Fail fast either way.
                if m.notify_on == NotifyOn::Condition {
                    parse_condition(&m.until)?;
                }
            }
            JobConfig::Shell(_) => {}
            JobConfig::Agent(_) => {
                if self.inner.agent_executor.get().is_none() {
                    bail!("Agent execution is not available");
                }
            }
        }
        if let Some(refusal) = self.refusal(
            &manifest.config,
            &manifest.principal(),
            manifest.timeout_secs,
        ) {
            bail!(refusal);
        }

        // Initialize retry state if policy is set.
        let mut manifest = manifest;
        if manifest.retry_policy.is_some() && manifest.retry_state.is_none() {
            manifest.retry_state = Some(RetryState::default());
        }

        // Persist the manifest.
        self.inner.persistence.save_manifest(&manifest).await?;

        // Add to jobs map.
        {
            let mut jobs = self.inner.jobs.write().await;
            jobs.insert(job_id.clone(), manifest.clone());
        }

        // Try to start immediately or queue.
        self.try_start_or_queue(manifest).await?;

        info!(job_id = %job_id, "Job submitted");
        Ok(job_id)
    }

    /// Submit a chain of jobs. Validates DAG, creates job manifests, and
    /// starts root jobs (those with no dependencies).
    pub async fn submit_chain(
        &self,
        steps: Vec<ChainStep>,
        agent_id: String,
        notify_channel: Option<String>,
        notify_thread: Option<String>,
        session_channel: Option<String>,
    ) -> Result<(ChainId, Vec<(String, JobId)>)> {
        // Validate the DAG.
        let edges = chain::validate_chain_dag(&steps)?;
        let submitter = crate::user_config::current_principal();
        if let Some((step, refusal)) = steps.iter().find_map(|step| {
            let timeout = step.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
            self.refusal(&step.config, &submitter, timeout)
                .map(|refusal| (&step.id, refusal))
        }) {
            bail!("Step '{step}': {refusal}");
        }

        let chain_id = format!("ch-{}", &uuid::Uuid::new_v4().to_string()[..8]);

        // Create job manifests for each step.
        let mut step_jobs = Vec::new();
        let mut step_id_to_job_id = Vec::new();

        for step in &steps {
            let job_type = match &step.config {
                JobConfig::Tool(_) => JobType::Tool,
                JobConfig::Shell(_) => JobType::Shell,
                JobConfig::Monitor(_) => JobType::Monitor,
                JobConfig::Agent(_) => JobType::Agent,
            };

            let timeout = step.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);

            let mut manifest = JobManifest::new(
                agent_id.clone(),
                job_type,
                step.config.clone(),
                step.label.clone().or_else(|| Some(step.id.clone())),
                timeout,
            );
            manifest.chain_id = Some(chain_id.clone());
            manifest.chain_step_id = Some(step.id.clone());
            manifest.notify_channel = notify_channel.clone();
            manifest.notify_thread = notify_thread.clone();
            manifest.session_channel = session_channel.clone();
            manifest.retry_policy = step.retry.clone();
            if manifest.retry_policy.is_some() {
                manifest.retry_state = Some(RetryState::default());
            }
            if step.stream {
                manifest.stream_config = Some(StreamConfig {
                    enabled: true,
                    throttle_secs: DEFAULT_STREAM_THROTTLE_SECS,
                });
            }

            // Persist the manifest.
            self.inner.persistence.save_manifest(&manifest).await?;

            step_jobs.push(StepJobMapping {
                step_id: step.id.clone(),
                job_id: manifest.job_id.clone(),
                status: JobStatus::Submitted,
            });
            step_id_to_job_id.push((step.id.clone(), manifest.job_id.clone()));

            // Add to jobs map.
            {
                let mut jobs = self.inner.jobs.write().await;
                jobs.insert(manifest.job_id.clone(), manifest);
            }
        }

        // Create and persist the chain.
        let chain = JobChain {
            id: chain_id.clone(),
            agent_id,
            step_jobs,
            edges,
            status: ChainStatus::Pending,
            submitted_at: Utc::now(),
            completed_at: None,
            session_channel,
            notify_channel,
            notify_thread,
        };
        self.inner.persistence.save_chain(&chain).await?;

        // Store in memory.
        {
            let mut chains = self.inner.chains.write().await;
            chains.insert(chain_id.clone(), chain);
        }

        // Start root jobs (those with no dependencies).
        self.advance_chain(&chain_id).await?;

        info!(chain_id = %chain_id, steps = step_id_to_job_id.len(), "Chain submitted");
        Ok((chain_id, step_id_to_job_id))
    }

    /// Advance a chain — enqueue any steps whose dependencies are all completed.
    /// Uses the queue + dequeue loop to avoid recursive futures.
    async fn advance_chain(&self, chain_id: &str) -> Result<()> {
        // Select ready steps and mark them Running in the same critical section.
        // `get_ready_steps` only returns `Submitted` steps, so marking dispatched
        // ones `Running` here stops a later/concurrent advance from re-queuing a
        // step that's already in flight (which would double-start it).
        let (ready_steps, snapshot) = {
            let mut chains = self.inner.chains.write().await;
            let Some(chain) = chains.get_mut(chain_id) else {
                return Ok(());
            };
            let ready = get_ready_steps(chain);
            for step_id in &ready {
                if let Some(job_id) = chain.job_id_for_step(step_id).map(str::to_string) {
                    chain.update_step_status(&job_id, JobStatus::Running);
                }
            }
            let snapshot = (!ready.is_empty()).then(|| chain.clone());
            (ready, snapshot)
        };

        if ready_steps.is_empty() {
            return Ok(());
        }
        if let Some(chain) = snapshot {
            let _ = self.inner.persistence.save_chain(&chain).await;
        }

        // Collect upstream context for ready steps and push them to the queue.
        for step_id in &ready_steps {
            let (job_id, upstream_context) = {
                let chains = self.inner.chains.read().await;
                let chain = match chains.get(chain_id) {
                    Some(c) => c,
                    None => continue,
                };

                let job_id = match chain.job_id_for_step(step_id) {
                    Some(id) => id.to_string(),
                    None => continue,
                };

                // Build upstream context from completed dependency outputs.
                let mut context = serde_json::Map::new();
                for upstream_step in chain.upstream_steps(step_id) {
                    if let Some(upstream_job_id) = chain.job_id_for_step(upstream_step) {
                        if let Ok(Some(output)) =
                            self.inner.persistence.load_output(upstream_job_id).await
                        {
                            context.insert(
                                upstream_step.to_string(),
                                serde_json::json!({
                                    "job_id": upstream_job_id,
                                    "output_summary": output.tail(20),
                                    "tool_result": output.tool_result,
                                    "exit_code": output.exit_code,
                                }),
                            );
                        }
                    }
                }

                let upstream = if context.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::Object(context))
                };

                (job_id, upstream)
            };

            // Update manifest with upstream context and push to queue.
            {
                let mut jobs = self.inner.jobs.write().await;
                if let Some(manifest) = jobs.get_mut(&job_id) {
                    manifest.upstream_context = upstream_context;
                    let manifest_clone = manifest.clone();
                    let _ = self.inner.persistence.save_manifest(&manifest_clone).await;

                    // Push to queue instead of calling try_start_or_queue directly.
                    let mut queue = self.inner.queue.lock().await;
                    queue.push_back(manifest_clone);
                }
            }
        }

        // Signal the dequeue loop to process the newly queued jobs.
        for _ in 0..ready_steps.len() {
            let _ = self.inner.dequeue_tx.try_send(());
        }

        Ok(())
    }

    /// Handle cascading cancellation when a step in a chain fails.
    async fn cascade_chain_failure(&self, chain: &mut JobChain, failed_step: &str) {
        let downstream = get_all_downstream(chain, failed_step);

        for step_id in &downstream {
            if let Some(step) = chain.step_jobs.iter_mut().find(|s| s.step_id == *step_id) {
                if !step.status.is_terminal() {
                    // Cancel the job.
                    let _ = self.cancel(&step.job_id).await;

                    // Update step status.
                    step.status = JobStatus::DependencyFailed;

                    // Update the job manifest.
                    let mut jobs = self.inner.jobs.write().await;
                    if let Some(manifest) = jobs.get_mut(&step.job_id) {
                        manifest.status = JobStatus::DependencyFailed;
                        manifest.error = Some(format!("dependency '{}' failed", failed_step));
                        manifest.completed_at = Some(Utc::now());
                        let _ = self.inner.persistence.save_manifest(manifest).await;
                    }
                }
            }
        }
    }

    /// Handle a job completion within a chain context.
    async fn handle_chain_job_completion(&self, chain_id: &str, job_id: &str, success: bool) {
        let mut chains = self.inner.chains.write().await;
        let chain = match chains.get_mut(chain_id) {
            Some(c) => c,
            None => return,
        };

        // Find the step and update its status.
        let step_id = match chain.step_id_for_job(job_id) {
            Some(id) => id.to_string(),
            None => return,
        };

        let new_status = if success {
            JobStatus::Completed
        } else {
            JobStatus::Failed
        };
        chain.update_step_status(job_id, new_status);

        if !success {
            // Cascade failure to downstream steps.
            drop(chains);

            let mut chains = self.inner.chains.write().await;
            if let Some(chain) = chains.get_mut(chain_id) {
                let downstream = get_all_downstream(chain, &step_id);
                for downstream_step in &downstream {
                    if let Some(step) = chain
                        .step_jobs
                        .iter_mut()
                        .find(|s| s.step_id == *downstream_step)
                    {
                        if !step.status.is_terminal() {
                            step.status = JobStatus::DependencyFailed;
                            // Cancel the actual job.
                            let _ = self.cancel(&step.job_id).await;

                            // Update the job manifest.
                            let mut jobs = self.inner.jobs.write().await;
                            if let Some(manifest) = jobs.get_mut(&step.job_id) {
                                manifest.status = JobStatus::DependencyFailed;
                                manifest.error = Some(format!("dependency '{}' failed", step_id));
                                manifest.completed_at = Some(Utc::now());
                                let _ = self.inner.persistence.save_manifest(manifest).await;
                            }
                        }
                    }
                }

                // Update chain status.
                chain.status = chain.compute_status();
                if chain.status.is_terminal() {
                    chain.completed_at = Some(Utc::now());
                    self.publish_chain_completion(chain).await;
                }
                let _ = self.inner.persistence.save_chain(chain).await;
            }
        } else {
            // Success — check if we can advance the chain.
            chain.status = chain.compute_status();
            if chain.status.is_terminal() {
                chain.completed_at = Some(Utc::now());
                self.publish_chain_completion(chain).await;
            }
            let _ = self.inner.persistence.save_chain(chain).await;
            drop(chains);

            // Advance: start newly ready steps via a separate task to avoid recursive futures.
            let runner = self.clone();
            let chain_id_owned = chain_id.to_string();
            tokio::spawn(async move {
                if let Err(e) = runner.advance_chain(&chain_id_owned).await {
                    warn!(chain_id = %chain_id_owned, error = %e, "Failed to advance chain");
                }
            });
        }
    }

    /// Publish chain completion notification via JetStream.
    async fn publish_chain_completion(&self, chain: &JobChain) {
        if let Some(ref publisher) = self.inner.publisher {
            let notification = serde_json::json!({
                "chain_id": chain.id,
                "agent_id": chain.agent_id,
                "status": chain.status,
                "steps": chain.step_jobs.iter().map(|s| serde_json::json!({
                    "step_id": s.step_id,
                    "job_id": s.job_id,
                    "status": s.status,
                })).collect::<Vec<_>>(),
                "submitted_at": chain.submitted_at,
                "completed_at": chain.completed_at,
                "timestamp": Utc::now(),
            });

            let subject = format!("workforce.jobs.{}.chain.completed", chain.agent_id);
            if let Err(e) = publish_json(publisher.as_ref(), subject, &notification).await {
                warn!(
                    chain_id = %chain.id,
                    error = %e,
                    "Failed to publish chain completion notification"
                );
            }
        }
    }

    /// Schedule a retry for a job after its backoff delay.
    fn schedule_retry(&self, manifest: JobManifest) {
        let remaining = retry::remaining_delay_secs(&manifest);
        let job_id = manifest.job_id.clone();
        let runner = self.clone();

        tokio::spawn(async move {
            if remaining > 0 {
                debug!(
                    job_id = %job_id,
                    delay_secs = remaining,
                    "Waiting for retry backoff"
                );
                tokio::time::sleep(std::time::Duration::from_secs(remaining)).await;
            }

            // Check if the job is still in retrying state (might have been cancelled).
            let should_execute = {
                let jobs = runner.inner.jobs.read().await;
                jobs.get(&job_id)
                    .map(|m| m.status == JobStatus::Retrying)
                    .unwrap_or(false)
            };

            if should_execute {
                // Reset status to submitted and queue for re-execution.
                {
                    let mut jobs = runner.inner.jobs.write().await;
                    if let Some(manifest) = jobs.get_mut(&job_id) {
                        manifest.status = JobStatus::Submitted;
                        let _ = runner.inner.persistence.save_manifest(manifest).await;

                        // Push to the queue and signal the dequeue loop.
                        let mut queue = runner.inner.queue.lock().await;
                        queue.push_back(manifest.clone());
                    }
                }
                let _ = runner.inner.dequeue_tx.try_send(());
            }
        });
    }

    /// Try to start a job immediately if a slot is available, otherwise queue it.
    async fn try_start_or_queue(&self, manifest: JobManifest) -> Result<()> {
        let permit = self.inner.semaphore.clone().try_acquire_owned();
        match permit {
            Ok(permit) => {
                self.start_job(manifest, permit).await?;
            }
            Err(_) => {
                debug!(
                    job_id = %manifest.job_id,
                    "No slot available, queueing job"
                );
                let mut queue = self.inner.queue.lock().await;
                queue.push_back(manifest);
            }
        }
        Ok(())
    }

    /// Start executing a job in a background task.
    async fn start_job(
        &self,
        mut manifest: JobManifest,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<()> {
        let job_id = manifest.job_id.clone();

        // Update status.
        manifest.started_at = Some(Utc::now());
        manifest.status = if matches!(manifest.config, JobConfig::Monitor(_)) {
            JobStatus::Monitoring
        } else {
            JobStatus::Running
        };

        self.inner.persistence.save_manifest(&manifest).await?;
        {
            let mut jobs = self.inner.jobs.write().await;
            jobs.insert(job_id.clone(), manifest.clone());
        }

        // Create cancellation channel.
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        // Create output streamer if streaming is configured.
        let streamer = if let Some(ref config) = manifest.stream_config {
            if config.enabled {
                let s = OutputStreamer::new(
                    job_id.clone(),
                    self.inner.agent_id.clone(),
                    config,
                    self.inner.publisher.clone(),
                    self.inner.session_injector.clone(),
                    manifest.session_channel.clone(),
                );
                // Store streamer for external toggling.
                {
                    let mut streamers = self.inner.streamers.write().await;
                    streamers.insert(job_id.clone(), s.clone());
                }
                Some(s)
            } else {
                None
            }
        } else {
            None
        };
        let streamer_for_task = streamer.clone();

        // Clone what the spawned task needs.
        let runner = self.clone();
        let job_id_clone = job_id.clone();

        let handle = tokio::spawn(async move {
            let _permit = permit; // Hold the permit until done.

            let principal = manifest.principal();
            let result = crate::user_config::as_principal(&principal, async {
                match &manifest.config {
                    JobConfig::Tool(t) => {
                        execute_tool_job(
                            &runner.inner.tool_executor,
                            &t.tool,
                            t.params.clone(),
                            cancel_rx,
                            manifest.timeout_secs,
                            streamer_for_task.as_ref(),
                        )
                        .await
                    }
                    JobConfig::Shell(s) => {
                        execute_shell_job(
                            &runner.inner.tool_executor,
                            &s.command,
                            s.workdir.as_deref(),
                            cancel_rx,
                            manifest.timeout_secs,
                            streamer_for_task.as_ref(),
                        )
                        .await
                    }
                    JobConfig::Monitor(m) => {
                        execute_monitor_job(
                            &runner.inner.tool_executor,
                            m,
                            MonitorDelivery {
                                sink: runner.inner.notification_sink.clone(),
                                channel: manifest.notify_channel.clone(),
                                thread: manifest.notify_thread.clone(),
                            },
                            cancel_rx,
                            manifest.timeout_secs,
                        )
                        .await
                    }
                    JobConfig::Agent(a) => {
                        execute_agent_job(
                            runner.inner.agent_executor.get(),
                            a,
                            &format!("agent-job:{}", manifest.job_id),
                            manifest.upstream_context.clone(),
                            cancel_rx,
                            manifest.timeout_secs,
                        )
                        .await
                    }
                }
            })
            .await;

            // Flush any remaining streamed output.
            if let Some(ref s) = streamer_for_task {
                s.flush().await;
            }

            // Update manifest with result.
            let (mut final_manifest, output) = match result {
                Ok((mut output, polls_completed)) => {
                    manifest.status = JobStatus::Completed;
                    manifest.completed_at = Some(Utc::now());
                    if let Some(exit) = output.exit_code {
                        manifest.exit_code = Some(exit);
                    }
                    output.job_id = manifest.job_id.clone();
                    // Update polls on monitor jobs.
                    if let JobConfig::Monitor(ref mut m) = manifest.config {
                        m.polls_completed = polls_completed.unwrap_or(m.polls_completed);
                    }
                    // Record successful attempt if retrying.
                    retry::record_success(&mut manifest);
                    (manifest, Some(output))
                }
                Err(e) => {
                    let error_str = format!("{:#}", e);
                    manifest.status = JobStatus::Failed;
                    manifest.completed_at = Some(Utc::now());
                    manifest.error = Some(error_str);
                    (manifest, None)
                }
            };

            // Check if we should retry on failure.
            let should_retry =
                final_manifest.status == JobStatus::Failed && retry::should_retry(&final_manifest);

            if should_retry {
                // Save attempt output before retrying.
                if let Some(ref out) = output {
                    let attempt = final_manifest
                        .retry_state
                        .as_ref()
                        .map(|s| s.current_attempt)
                        .unwrap_or(1);
                    let _ = runner
                        .inner
                        .persistence
                        .save_attempt_output(out, attempt)
                        .await;
                }

                // Prepare for retry.
                retry::prepare_retry(&mut final_manifest, None);

                // Persist updated manifest.
                if let Err(e) = runner
                    .inner
                    .persistence
                    .save_manifest(&final_manifest)
                    .await
                {
                    error!(job_id = %job_id_clone, error = %e, "Failed to save retry manifest");
                }

                // Update in-memory map.
                {
                    let mut jobs = runner.inner.jobs.write().await;
                    jobs.insert(job_id_clone.clone(), final_manifest.clone());
                }

                // Remove handle from tracking.
                {
                    let mut handles = runner.inner.handles.lock().await;
                    handles.remove(&job_id_clone);
                }

                // Remove streamer.
                {
                    let mut streamers = runner.inner.streamers.write().await;
                    streamers.remove(&job_id_clone);
                }

                // Signal dequeue and schedule retry.
                let _ = runner.inner.dequeue_tx.try_send(());
                runner.schedule_retry(final_manifest);

                info!(job_id = %job_id_clone, "Job scheduled for retry");
                return;
            }

            // Persist final state.
            if let Err(e) = runner
                .inner
                .persistence
                .save_manifest(&final_manifest)
                .await
            {
                error!(job_id = %job_id_clone, error = %e, "Failed to save completed manifest");
            }
            if let Some(ref out) = output {
                if let Err(e) = runner.inner.persistence.save_output(out).await {
                    error!(job_id = %job_id_clone, error = %e, "Failed to save job output");
                }
                // Also save as attempt output if retried.
                if let Some(ref state) = final_manifest.retry_state {
                    if state.current_attempt > 1 || !state.attempts.is_empty() {
                        let _ = runner
                            .inner
                            .persistence
                            .save_attempt_output(out, state.current_attempt)
                            .await;
                    }
                }
            }

            // Publish JetStream notification (if publisher available).
            // Only publish on final outcome (success, or final failure after all retries).
            let notification = build_notification(&final_manifest, output.as_ref());
            if let Some(ref publisher) = runner.inner.publisher {
                let subject = format!("workforce.jobs.{}.completed", runner.inner.agent_id);
                if let Err(e) = publish_json(publisher.as_ref(), subject, &notification).await {
                    warn!(
                        job_id = %job_id_clone,
                        error = %e,
                        "Failed to publish JetStream job notification (non-fatal)"
                    );
                }
            }

            // Send notification to channel surface (if sink available and channel set).
            // A change-watch that ended gracefully already delivered its updates
            // mid-loop, so suppress the redundant completion notification.
            if let Some(ref sink) = runner.inner.notification_sink {
                if let Some(channel) = final_manifest.notify_channel.as_ref().filter(|_| {
                    !(final_manifest.status == JobStatus::Completed
                        && matches!(&final_manifest.config, JobConfig::Monitor(m) if m.notify_on == NotifyOn::Change))
                }) {
                    let text = format_completion_message(&final_manifest, output.as_ref());
                    if let Err(e) = sink
                        .send_notification(channel, final_manifest.notify_thread.as_deref(), &text)
                        .await
                    {
                        warn!(
                            job_id = %job_id_clone,
                            error = %e,
                            "Failed to send channel notification"
                        );
                    }
                }
            }

            // Record job metrics in the stats/telemetry system.
            if let Some(ref recorder) = runner.inner.stats_recorder {
                recorder
                    .record_job_telemetry(&final_manifest, output.as_ref())
                    .await;
            }

            // Inject completion notification into the originating agent session.
            if let Some(ref injector) = runner.inner.session_injector {
                if let Some(ref session_channel) = final_manifest.session_channel {
                    let text = format_completion_message(&final_manifest, output.as_ref());
                    match injector.inject_job_completion(session_channel, &text).await {
                        Ok(true) => {
                            debug!(
                                job_id = %job_id_clone,
                                session = %session_channel,
                                "Injected job completion into active session"
                            );
                        }
                        Ok(false) => {
                            debug!(
                                job_id = %job_id_clone,
                                session = %session_channel,
                                "Session not active, completion stored for retrieval"
                            );
                        }
                        Err(e) => {
                            warn!(
                                job_id = %job_id_clone,
                                error = %e,
                                "Failed to inject job completion into session"
                            );
                        }
                    }
                }
            }

            // Update in-memory map.
            {
                let mut jobs = runner.inner.jobs.write().await;
                jobs.insert(job_id_clone.clone(), final_manifest.clone());
            }

            // Remove handle from tracking.
            {
                let mut handles = runner.inner.handles.lock().await;
                handles.remove(&job_id_clone);
            }

            // Remove streamer.
            {
                let mut streamers = runner.inner.streamers.write().await;
                streamers.remove(&job_id_clone);
            }

            // Handle chain advancement if this job is part of a chain.
            if let Some(ref chain_id) = final_manifest.chain_id {
                let success = final_manifest.status == JobStatus::Completed;
                runner
                    .handle_chain_job_completion(chain_id, &job_id_clone, success)
                    .await;
            }

            // Signal the dequeue loop to try starting queued jobs.
            let _ = runner.inner.dequeue_tx.try_send(());

            info!(job_id = %job_id_clone, "Job completion processed");
        });

        // Store the handle.
        {
            let mut handles = self.inner.handles.lock().await;
            handles.insert(
                job_id,
                RunningJob {
                    handle,
                    cancel: cancel_tx,
                    streamer,
                },
            );
        }

        Ok(())
    }

    /// Every agent job this agent kept that runs as or bills anyone in
    /// `names`, newest first.
    pub async fn jobs_for(&self, names: &[String]) -> Result<Vec<JobManifest>> {
        Ok(self
            .inner
            .persistence
            .scan_all_manifests()
            .await?
            .into_iter()
            .filter(|manifest| agent_job_names(manifest, names))
            .collect())
    }

    /// Every running or queued job of any kind that runs as anyone in
    /// `names`: an agent job's person, or whoever submitted any other.
    pub async fn active_run_as(&self, names: &[String]) -> Result<Vec<JobManifest>> {
        Ok(self
            .inner
            .persistence
            .scan_all_manifests()
            .await?
            .into_iter()
            .filter(|manifest| {
                manifest.status.is_active()
                    && manifest
                        .principal()
                        .run_as
                        .is_some_and(|person| names.contains(&person))
            })
            .collect())
    }

    /// How many agent jobs this agent kept run as each person: the ones a
    /// delete removes.
    pub async fn people(&self) -> Result<std::collections::BTreeMap<String, usize>> {
        Ok(self
            .inner
            .persistence
            .scan_all_manifests()
            .await?
            .into_iter()
            .filter_map(|manifest| match manifest.config {
                JobConfig::Agent(job) => job.user_id,
                _ => None,
            })
            .fold(std::collections::BTreeMap::new(), |mut people, id| {
                *people.entry(id).or_default() += 1;
                people
            }))
    }

    /// Make every agent job naming anyone in `names` run as and bill `to`.
    /// Returns how many changed.
    pub async fn reassign_person(&self, names: &[String], to: &str) -> Result<usize> {
        let mut changed = 0;
        let mut jobs = self.inner.jobs.write().await;
        for mut manifest in self.jobs_for(names).await? {
            let live = jobs.get_mut(&manifest.job_id);
            let target = match live {
                Some(live) => live,
                None => &mut manifest,
            };
            if let JobConfig::Agent(job) = &mut target.config {
                for slot in [&mut job.user_id, &mut job.bill_to] {
                    if slot.as_ref().is_some_and(|id| names.contains(id)) {
                        *slot = Some(to.to_string());
                    }
                }
            }
            self.inner.persistence.save_manifest(target).await?;
            changed += 1;
        }
        Ok(changed)
    }

    /// Remove every finished agent job run as anyone in `names`, with its
    /// task and outputs; one that only bills them is someone else's and
    /// stays. A job still running is cancelled and named in the second value
    /// instead, for a later pass to remove once it has stopped.
    pub async fn forget_person(&self, names: &[String]) -> Result<(usize, Vec<String>)> {
        let mut removed = 0;
        let mut running = Vec::new();
        for manifest in self.jobs_for(names).await?.into_iter().filter(|manifest| {
            matches!(&manifest.config, JobConfig::Agent(job)
                if job.user_id.as_ref().is_some_and(|id| names.contains(id)))
        }) {
            if manifest.status.is_active() {
                self.cancel(&manifest.job_id).await?;
                running.push(manifest.job_id);
                continue;
            }
            self.inner.jobs.write().await.remove(&manifest.job_id);
            self.inner.persistence.remove_job(&manifest.job_id).await?;
            removed += 1;
        }
        Ok((removed, running))
    }

    /// Cancel a running job.
    pub async fn cancel(&self, job_id: &str) -> Result<bool> {
        // Send cancellation signal.
        {
            let handles = self.inner.handles.lock().await;
            if let Some(job) = handles.get(job_id) {
                let _ = job.cancel.send(true);
            }
        }

        // Update manifest.
        {
            let mut jobs = self.inner.jobs.write().await;
            if let Some(manifest) = jobs.get_mut(job_id) {
                if manifest.status.is_active() {
                    manifest.status = JobStatus::Cancelled;
                    manifest.completed_at = Some(Utc::now());
                    let _ = self.inner.persistence.save_manifest(manifest).await;
                    return Ok(true);
                }
            }
        }

        // Check on disk if not in memory.
        if let Ok(Some(mut manifest)) = self.inner.persistence.load_manifest(job_id).await {
            if manifest.status.is_active() {
                manifest.status = JobStatus::Cancelled;
                manifest.completed_at = Some(Utc::now());
                let _ = self.inner.persistence.save_manifest(&manifest).await;
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Cancel all jobs in a chain.
    pub async fn cancel_chain(&self, chain_id: &str) -> Result<bool> {
        let mut chains = self.inner.chains.write().await;
        let chain = match chains.get_mut(chain_id) {
            Some(c) => c,
            None => {
                // Try loading from disk.
                match self.inner.persistence.load_chain(chain_id).await? {
                    Some(c) => {
                        chains.insert(chain_id.to_string(), c);
                        chains.get_mut(chain_id).unwrap()
                    }
                    None => return Ok(false),
                }
            }
        };

        if chain.status.is_terminal() {
            return Ok(false);
        }

        // Cancel all non-terminal jobs.
        let job_ids: Vec<String> = chain
            .step_jobs
            .iter()
            .filter(|s| !s.status.is_terminal())
            .map(|s| s.job_id.clone())
            .collect();

        for job_id in &job_ids {
            let _ = self.cancel(job_id).await;
        }

        // Update step statuses.
        for step in &mut chain.step_jobs {
            if !step.status.is_terminal() {
                step.status = JobStatus::Cancelled;
            }
        }

        chain.status = ChainStatus::Cancelled;
        chain.completed_at = Some(Utc::now());
        let _ = self.inner.persistence.save_chain(chain).await;

        Ok(true)
    }

    /// Toggle streaming on/off for a running job.
    pub async fn toggle_stream(&self, job_id: &str, enabled: bool) -> Result<bool> {
        let streamers = self.inner.streamers.read().await;
        if let Some(streamer) = streamers.get(job_id) {
            streamer.set_enabled(enabled);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Get the stream status for a job.
    pub async fn stream_status(&self, job_id: &str) -> Option<bool> {
        let streamers = self.inner.streamers.read().await;
        streamers.get(job_id).map(|s| s.is_enabled())
    }

    /// List the jobs `reader` may see, optionally filtered by status.
    pub async fn list(&self, status_filter: Option<&str>, reader: &JobReader) -> Vec<JobSummary> {
        let jobs = self.inner.jobs.read().await;
        let mut summaries: Vec<JobSummary> = jobs
            .values()
            .filter(|m| match status_filter {
                Some("all") | None => true,
                Some(filter) => m.status.to_string() == filter,
            })
            .filter(|m| m.readable_by(reader))
            .map(JobSummary::from)
            .collect();

        // Include disk-persisted terminal jobs not in memory.
        if let Ok(disk_manifests) = self.inner.persistence.scan_all_manifests().await {
            for dm in disk_manifests {
                if !jobs.contains_key(&dm.job_id) {
                    let include = match status_filter {
                        Some("all") | None => true,
                        Some(filter) => dm.status.to_string() == filter,
                    };
                    if include && dm.readable_by(reader) {
                        summaries.push(JobSummary::from(&dm));
                    }
                }
            }
        }

        summaries.sort_by(|a, b| b.submitted_at.cmp(&a.submitted_at));
        summaries
    }

    /// List the chains `reader` may see, optionally filtered by status. A
    /// chain is its jobs, so one with a job the reader may not see is not
    /// theirs.
    pub async fn list_chains(
        &self,
        status_filter: Option<&str>,
        reader: &JobReader,
    ) -> Vec<ChainSummary> {
        let wanted = |status: &ChainStatus| match status_filter {
            Some("all") | None => true,
            Some(filter) => status.to_string() == filter,
        };
        let mut found: Vec<ChainSummary> = {
            let chains = self.inner.chains.read().await;
            let on_disk = self
                .inner
                .persistence
                .scan_all_chains()
                .await
                .unwrap_or_default();
            chains
                .values()
                .chain(on_disk.iter().filter(|c| !chains.contains_key(&c.id)))
                .filter(|c| wanted(&c.status))
                .map(ChainSummary::from_chain)
                .collect()
        };
        found.sort_by(|a, b| b.submitted_at.cmp(&a.submitted_at));

        let mut summaries = Vec::with_capacity(found.len());
        for summary in found {
            if self.chain_readable_by(&summary, reader).await {
                summaries.push(summary);
            }
        }
        summaries
    }

    /// The chain `chain_id` names, when `reader` may see it.
    pub async fn chain_summary_for(
        &self,
        chain_id: &str,
        reader: &JobReader,
    ) -> Option<ChainSummary> {
        let summary = self.chain_summary(chain_id).await?;
        self.chain_readable_by(&summary, reader)
            .await
            .then_some(summary)
    }

    async fn chain_readable_by(&self, chain: &ChainSummary, reader: &JobReader) -> bool {
        for step in &chain.steps {
            if !self.readable_by(&step.job_id, reader).await {
                return false;
            }
        }
        true
    }

    /// Whether `reader` may see `job_id`. An unknown job reads as visible,
    /// so the action asked of it reports it missing.
    pub async fn readable_by(&self, job_id: &str, reader: &JobReader) -> bool {
        *reader == JobReader::Admin
            || self
                .manifest(job_id)
                .await
                .is_none_or(|manifest| manifest.readable_by(reader))
    }

    /// A job's manifest, from disk when the job is no longer held in memory.
    async fn manifest(&self, job_id: &str) -> Option<JobManifest> {
        match self.inner.jobs.read().await.get(job_id) {
            Some(manifest) => Some(manifest.clone()),
            None => self
                .inner
                .persistence
                .load_manifest(job_id)
                .await
                .ok()
                .flatten(),
        }
    }

    /// Summary of a single chain (in-memory, falling back to disk), for progress display.
    pub async fn chain_summary(&self, chain_id: &str) -> Option<ChainSummary> {
        if let Some(chain) = self.inner.chains.read().await.get(chain_id) {
            return Some(ChainSummary::from_chain(chain));
        }
        self.inner
            .persistence
            .load_chain(chain_id)
            .await
            .ok()
            .flatten()
            .map(|c| ChainSummary::from_chain(&c))
    }

    /// Get the full result of a completed job.
    pub async fn get_result(&self, job_id: &str, tail: Option<usize>) -> Result<serde_json::Value> {
        let manifest = {
            let jobs = self.inner.jobs.read().await;
            match jobs.get(job_id) {
                Some(m) => m.clone(),
                None => match self.inner.persistence.load_manifest(job_id).await? {
                    Some(m) => m,
                    None => bail!("Job '{}' not found", job_id),
                },
            }
        };

        let output = self.inner.persistence.load_output(job_id).await?;

        let mut result = serde_json::json!({
            "job_id": manifest.job_id,
            "status": manifest.status,
            "job_type": manifest.job_type,
            "label": manifest.label,
            "elapsed_secs": manifest.elapsed_secs(),
            "elapsed_display": manifest.elapsed_display(),
            "submitted_at": manifest.submitted_at,
            "started_at": manifest.started_at,
            "completed_at": manifest.completed_at,
            "exit_code": manifest.exit_code,
            "error": manifest.error,
        });

        // Add chain info.
        if let Some(ref chain_id) = manifest.chain_id {
            result["chain_id"] = serde_json::json!(chain_id);
            result["chain_step_id"] = serde_json::json!(manifest.chain_step_id);
        }

        // Add retry info.
        if let Some(ref state) = manifest.retry_state {
            result["retry"] = serde_json::json!({
                "current_attempt": state.current_attempt,
                "max_attempts": manifest.retry_policy.as_ref().map(|p| p.max_attempts),
                "attempts": retry::format_attempts(&manifest),
            });
        }

        if let Some(output) = output {
            let output_text = if let Some(n) = tail {
                output.tail(n)
            } else {
                output.full_output.clone()
            };

            result["output"] = serde_json::json!({
                "text": output_text,
                "tool_result": output.tool_result,
                "stdout": output.stdout,
                "stderr": output.stderr,
                "exit_code": output.exit_code,
            });
        }

        Ok(result)
    }

    /// Full manifests for jobs of a given type, most-recently-submitted first.
    pub async fn list_by_type(&self, job_type: JobType) -> Vec<JobManifest> {
        let jobs = self.inner.jobs.read().await;
        let mut out: Vec<JobManifest> = jobs
            .values()
            .filter(|m| m.job_type == job_type)
            .cloned()
            .collect();
        out.sort_by(|a, b| b.submitted_at.cmp(&a.submitted_at));
        out
    }

    /// [`Self::job_detail`] for a job `reader` may see; `None` for one that
    /// is unknown or somebody else's, which a caller reports alike.
    pub async fn job_detail_for(
        &self,
        job_id: &str,
        reader: &JobReader,
    ) -> Option<(JobManifest, Option<JobOutput>)> {
        self.job_detail(job_id)
            .await
            .filter(|(manifest, _)| manifest.readable_by(reader))
    }

    /// A job's manifest plus its completion output, loading from disk when the
    /// job is no longer held in memory. `None` if the job is unknown.
    pub async fn job_detail(&self, job_id: &str) -> Option<(JobManifest, Option<JobOutput>)> {
        let manifest = self.manifest(job_id).await?;
        let output = self
            .inner
            .persistence
            .load_output(job_id)
            .await
            .ok()
            .flatten();
        Some((manifest, output))
    }

    /// Get the count of active (non-terminal) jobs.
    pub async fn active_count(&self) -> usize {
        let jobs = self.inner.jobs.read().await;
        jobs.values().filter(|m| m.status.is_active()).count()
    }

    /// Get the queue depth.
    pub async fn queue_depth(&self) -> usize {
        let queue = self.inner.queue.lock().await;
        queue.len()
    }
}

// ── Job Execution Functions ──────────────────────────────────────────

/// Execute a tool job.
async fn execute_tool_job(
    executor: &Arc<dyn ToolExecutor>,
    tool_name: &str,
    params: serde_json::Value,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    timeout_secs: u64,
    streamer: Option<&OutputStreamer>,
) -> Result<(JobOutput, Option<u64>)> {
    let result = tokio::select! {
        r = executor.execute_tool(tool_name, params) => r?,
        _ = cancel_rx.changed() => {
            bail!("Job cancelled");
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {
            bail!("Job timed out after {}s", timeout_secs);
        }
    };

    let full_output = if result.success {
        serde_json::to_string_pretty(&result.output).unwrap_or_default()
    } else {
        result
            .error
            .clone()
            .unwrap_or_else(|| "Unknown error".to_string())
    };

    // Stream the output if streamer is available.
    if let Some(s) = streamer {
        let lines: Vec<String> = full_output.lines().map(|l| l.to_string()).collect();
        s.write_lines(&lines).await;
    }

    Ok((
        JobOutput {
            job_id: String::new(),
            tool_result: Some(result.output),
            stdout: None,
            stderr: None,
            exit_code: None,
            full_output,
            attempt: None,
        },
        None,
    ))
}

/// Execute an agent job by running an isolated sub-agent.
/// Append upstream chain steps' staged findings handles to an agent task, so a
/// synthesiser step can `data.query` its workers' findings. Returns the task
/// unchanged when there are no upstream findings (the common, non-chain case).
fn augment_agent_task(task: &str, upstream: Option<&serde_json::Value>) -> String {
    let Some(steps) = upstream.and_then(|v| v.as_object()) else {
        return task.to_string();
    };
    let mut handles: Vec<(&String, &str)> = steps
        .iter()
        .filter_map(|(step, v)| {
            let id = v.get("tool_result")?.get("findings")?.get("artifact_id")?;
            Some((step, id.as_str()?))
        })
        .collect();
    if handles.is_empty() {
        return task.to_string();
    }
    handles.sort_by(|a, b| a.0.cmp(b.0));
    let list = handles
        .iter()
        .map(|(step, id)| format!("- {step}: {id}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{task}\n\nThe investigators staged these findings artifacts. Read them with \
         `data.query` (pass the ids in `inputs`) to dedup, rank, and synthesise — never \
         page through them:\n{list}"
    )
}

async fn execute_agent_job(
    executor: Option<&Arc<dyn AgentExecutor>>,
    config: &AgentJobConfig,
    channel: &str,
    upstream: Option<serde_json::Value>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    timeout_secs: u64,
) -> Result<(JobOutput, Option<u64>)> {
    let executor = executor.ok_or_else(|| eyre::eyre!("Agent execution is not available"))?;

    let (task, ledger) = match &config.research {
        Some(role) => crate::research_ledger::prepare(role, &config.task, upstream.as_ref()),
        None => (augment_agent_task(&config.task, upstream.as_ref()), None),
    };
    let config = AgentJobConfig {
        task,
        ..config.clone()
    };

    let outcome = tokio::select! {
        r = executor.run_agent_job(&config, channel) => r,
        _ = cancel_rx.changed() => bail!("Job cancelled"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {
            Err(eyre::eyre!("Job timed out after {timeout_secs}s"))
        }
    };
    let result = match (outcome, ledger) {
        // A synthesiser's report is delivered through its ledger, which also
        // stands in for a synthesis that failed.
        (outcome, Some(ledger)) => AgentRunResult {
            response: ledger.deliver(
                outcome
                    .as_ref()
                    .map(|r| r.response.clone())
                    .map_err(|e| e.to_string()),
            ),
            ..outcome.unwrap_or_default()
        },
        (Ok(result), None) => result,
        // A worker is a resilient fan-out unit: a failed or timed-out run completes
        // empty so it doesn't cascade-fail the synthesiser. Other profiles propagate,
        // and so does a worker that owed a package: completing without one would
        // read as a hand-over that happened.
        (Err(e), None) if config.profile == AgentProfile::Worker && config.handoff.is_none() => {
            AgentRunResult {
                response: format!("Worker could not complete: {e}"),
                ..AgentRunResult::default()
            }
        }
        (Err(e), None) => return Err(e),
    };

    Ok((
        JobOutput {
            job_id: String::new(),
            tool_result: result.tool_result(),
            stdout: None,
            stderr: None,
            exit_code: None,
            full_output: result.response,
            attempt: None,
        },
        None,
    ))
}

/// Execute a shell job by wrapping exec.run.
/// The `exec.run` call a shell job makes.
fn shell_params(command: &str, workdir: Option<&str>, timeout_secs: u64) -> serde_json::Value {
    let mut params = serde_json::json!({
        "command": command,
        "timeout_secs": timeout_secs,
    });
    if let Some(wd) = workdir {
        params["workdir"] = serde_json::Value::String(wd.to_string());
    }
    params
}

async fn execute_shell_job(
    executor: &Arc<dyn ToolExecutor>,
    command: &str,
    workdir: Option<&str>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    timeout_secs: u64,
    streamer: Option<&OutputStreamer>,
) -> Result<(JobOutput, Option<u64>)> {
    let params = shell_params(command, workdir, timeout_secs);

    let result = tokio::select! {
        r = executor.execute_tool("exec.run", params) => r?,
        _ = cancel_rx.changed() => {
            bail!("Job cancelled");
        }
    };

    let stdout = result
        .output
        .get("stdout")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let stderr = result
        .output
        .get("stderr")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let exit_code = result
        .output
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .map(|n| n as i32);

    let mut full_output = String::new();
    if let Some(ref out) = stdout {
        if !out.is_empty() {
            full_output.push_str(out);
        }
    }
    if let Some(ref err) = stderr {
        if !err.is_empty() {
            if !full_output.is_empty() {
                full_output.push('\n');
            }
            full_output.push_str("STDERR:\n");
            full_output.push_str(err);
        }
    }

    // Stream the output if streamer is available.
    if let Some(s) = streamer {
        let lines: Vec<String> = full_output.lines().map(|l| l.to_string()).collect();
        s.write_lines(&lines).await;
    }

    Ok((
        JobOutput {
            job_id: String::new(),
            tool_result: Some(result.output),
            stdout,
            stderr,
            exit_code,
            full_output,
            attempt: None,
        },
        None,
    ))
}

/// Channel-delivery context a `Change` monitor uses to push mid-loop updates.
struct MonitorDelivery {
    sink: Option<Arc<dyn NotificationSink>>,
    channel: Option<String>,
    thread: Option<String>,
}

/// Execute a monitor job. `Condition` mode polls until `until` is true then
/// finishes; `Change` mode notifies on each change of the watched value and
/// keeps watching until max-polls/timeout, then finishes gracefully.
async fn execute_monitor_job(
    executor: &Arc<dyn ToolExecutor>,
    config: &MonitorJobConfig,
    delivery: MonitorDelivery,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    timeout_secs: u64,
) -> Result<(JobOutput, Option<u64>)> {
    let condition = match config.notify_on {
        NotifyOn::Condition => Some(parse_condition(&config.until)?),
        NotifyOn::Change => None,
    };
    let start_time = std::time::Instant::now();
    let mut polls = config.polls_completed;
    let mut last_seen: Option<serde_json::Value> = None;
    let mut changes: u64 = 0;

    loop {
        if *cancel_rx.borrow() {
            bail!("Job cancelled");
        }

        let timed_out = start_time.elapsed().as_secs() > timeout_secs;
        if timed_out || polls >= config.max_polls {
            match config.notify_on {
                // A change-watch ending its run is success, not failure.
                NotifyOn::Change => return Ok((watch_summary_output(polls, changes), Some(polls))),
                NotifyOn::Condition if timed_out => {
                    bail!(
                        "Monitor timed out after {}s ({} polls)",
                        timeout_secs,
                        polls
                    )
                }
                NotifyOn::Condition => {
                    bail!(
                        "Monitor exceeded max polls ({}/{})",
                        polls,
                        config.max_polls
                    )
                }
            }
        }

        let result = executor
            .execute_tool(&config.tool, config.params.clone())
            .await?;
        polls += 1;

        match config.notify_on {
            NotifyOn::Condition => {
                if condition
                    .as_ref()
                    .is_some_and(|c| c.evaluate(&result.output))
                {
                    let full_output =
                        serde_json::to_string_pretty(&result.output).unwrap_or_default();
                    return Ok((monitor_output(result.output, full_output), Some(polls)));
                }
            }
            NotifyOn::Change => {
                let observed = match &config.watch_path {
                    Some(path) => resolve_path(&result.output, path)
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    None => result.output.clone(),
                };
                if let Some(prev) = &last_seen {
                    if *prev != observed {
                        changes += 1;
                        notify_change(&delivery, &config.tool, prev, &observed).await;
                    }
                }
                last_seen = Some(observed);
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(config.interval_secs)) => {}
            _ = cancel_rx.changed() => {
                bail!("Job cancelled");
            }
        }
    }
}

fn monitor_output(tool_result: serde_json::Value, full_output: String) -> JobOutput {
    JobOutput {
        job_id: String::new(),
        tool_result: Some(tool_result),
        stdout: None,
        stderr: None,
        exit_code: None,
        full_output,
        attempt: None,
    }
}

fn watch_summary_output(polls: u64, changes: u64) -> JobOutput {
    monitor_output(
        serde_json::json!({ "polls": polls, "changes": changes }),
        format!("Watch ended after {polls} polls ({changes} change(s) reported)."),
    )
}

/// Push a `Change` monitor's update to its channel (a no-op without a sink/channel).
async fn notify_change(
    delivery: &MonitorDelivery,
    tool: &str,
    prev: &serde_json::Value,
    now: &serde_json::Value,
) {
    let (Some(sink), Some(channel)) = (&delivery.sink, &delivery.channel) else {
        return;
    };
    let text = format!(
        "👀 *Watch update* — `{tool}` changed\nfrom: {}\nto: {}",
        truncate_value(prev),
        truncate_value(now),
    );
    if let Err(e) = sink
        .send_notification(channel, delivery.thread.as_deref(), &text)
        .await
    {
        warn!(error = %e, tool, "Failed to deliver watch change notification");
    }
}

fn truncate_value(v: &serde_json::Value) -> String {
    let s = v.to_string();
    if s.len() > 300 {
        format!("{}…", &s[..s.floor_char_boundary(297)])
    } else {
        s
    }
}

// ── Notification Building ────────────────────────────────────────────

fn build_notification(manifest: &JobManifest, output: Option<&JobOutput>) -> JobNotification {
    let output_summary = output
        .map(|o| o.tail(NOTIFICATION_TAIL_LINES))
        .unwrap_or_default();

    let polls = match &manifest.config {
        JobConfig::Monitor(m) => Some(m.polls_completed),
        _ => None,
    };

    JobNotification {
        job_id: manifest.job_id.clone(),
        agent_id: manifest.agent_id.clone(),
        job_type: manifest.job_type.clone(),
        label: manifest.label.clone(),
        status: manifest.status.clone(),
        success: manifest.status == JobStatus::Completed,
        elapsed_secs: manifest.elapsed_secs(),
        output_summary,
        exit_code: manifest.exit_code,
        error: manifest.error.clone(),
        polls,
        session_channel: manifest.session_channel.clone(),
        notify_channel: manifest.notify_channel.clone(),
        notify_thread: manifest.notify_thread.clone(),
        timestamp: Utc::now(),
        chain_id: manifest.chain_id.clone(),
        retry_info: manifest.retry_display(),
    }
}

/// Format a human-readable completion message for channel delivery.
fn format_completion_message(manifest: &JobManifest, output: Option<&JobOutput>) -> String {
    let status_emoji = match manifest.status {
        JobStatus::Completed => "✅",
        JobStatus::Failed => "❌",
        JobStatus::Cancelled => "🚫",
        JobStatus::DependencyFailed => "⛓️‍💥",
        _ => "ℹ️",
    };

    let label = manifest.label.as_deref().unwrap_or(&manifest.job_id);

    let elapsed = manifest.elapsed_display();
    let type_str = &manifest.job_type;

    let mut msg = format!(
        "{} *Job complete* — _{}_\n`{}` • {} • {}",
        status_emoji, label, manifest.job_id, type_str, elapsed
    );

    // Add retry info.
    if let Some(retry_info) = manifest.retry_display() {
        msg.push_str(&format!(" • {}", retry_info));
    }

    // Add chain info.
    if let Some(ref chain_id) = manifest.chain_id {
        msg.push_str(&format!("\nChain: `{}`", chain_id));
        if let Some(ref step_id) = manifest.chain_step_id {
            msg.push_str(&format!(" (step: {})", step_id));
        }
    }

    if let Some(ref error) = manifest.error {
        let truncated = if error.len() > 500 {
            format!("{}…", &error[..error.floor_char_boundary(497)])
        } else {
            error.clone()
        };
        msg.push_str(&format!("\n> Error: {}", truncated));
    } else if let Some(output) = output {
        // Prose sub-agent responses read best from the head; log-like tool and
        // shell output from the tail.
        let summary = match manifest.job_type {
            JobType::Agent => output.full_output.clone(),
            _ => output.tail(10),
        };
        if !summary.is_empty() {
            let truncated = if summary.len() > 500 {
                format!("{}…", &summary[..summary.floor_char_boundary(497)])
            } else {
                summary
            };
            msg.push_str(&format!("\n```\n{}\n```", truncated));
        }
    }

    if let Some(code) = manifest.exit_code {
        msg.push_str(&format!("\nExit code: {}", code));
    }

    msg
}

#[async_trait::async_trait]
impl crate::hosts::JobHost for JobRunner {
    async fn submit(&self, manifest: JobManifest) -> Result<JobId> {
        JobRunner::submit(self, manifest).await
    }

    async fn submit_chain(
        &self,
        steps: Vec<ChainStep>,
        agent_id: String,
        notify_channel: Option<String>,
        notify_thread: Option<String>,
        session_channel: Option<String>,
    ) -> Result<(ChainId, Vec<(String, JobId)>)> {
        JobRunner::submit_chain(
            self,
            steps,
            agent_id,
            notify_channel,
            notify_thread,
            session_channel,
        )
        .await
    }

    async fn get_result(&self, job_id: &str, tail: Option<usize>) -> Result<serde_json::Value> {
        JobRunner::get_result(self, job_id, tail).await
    }

    async fn chain_summary(&self, chain_id: &str) -> Option<ChainSummary> {
        JobRunner::chain_summary(self, chain_id).await
    }

    async fn job_detail_for(
        &self,
        job_id: &str,
        reader: &JobReader,
    ) -> Option<(JobManifest, Option<JobOutput>)> {
        JobRunner::job_detail_for(self, job_id, reader).await
    }

    async fn list_by_type(&self, job_type: JobType) -> Vec<JobManifest> {
        JobRunner::list_by_type(self, job_type).await
    }

    async fn readable_by(&self, job_id: &str, reader: &JobReader) -> bool {
        JobRunner::readable_by(self, job_id, reader).await
    }

    async fn cancel(&self, job_id: &str) -> Result<bool> {
        JobRunner::cancel(self, job_id).await
    }

    async fn cancel_chain(&self, chain_id: &str) -> Result<bool> {
        JobRunner::cancel_chain(self, chain_id).await
    }

    async fn jobs_for(&self, names: &[String]) -> Result<Vec<JobManifest>> {
        JobRunner::jobs_for(self, names).await
    }

    async fn active_run_as(&self, names: &[String]) -> Result<Vec<JobManifest>> {
        JobRunner::active_run_as(self, names).await
    }

    async fn people(&self) -> Result<std::collections::BTreeMap<String, usize>> {
        JobRunner::people(self).await
    }

    async fn reassign_person(&self, names: &[String], to: &str) -> Result<usize> {
        JobRunner::reassign_person(self, names, to).await
    }

    async fn forget_person(&self, names: &[String]) -> Result<(usize, Vec<String>)> {
        JobRunner::forget_person(self, names).await
    }

    fn set_agent_executor(&self, executor: Arc<dyn AgentExecutor>) {
        JobRunner::set_agent_executor(self, executor)
    }

    async fn cache_stats(&self) -> JobRunnerCacheStats {
        JobRunner::cache_stats(self).await
    }
}

#[cfg(test)]
mod agent_job_tests {
    use super::*;
    use std::time::Duration;

    struct StubToolExecutor;

    #[async_trait::async_trait]
    impl ToolExecutor for StubToolExecutor {
        async fn execute_tool(
            &self,
            _tool: &str,
            _params: serde_json::Value,
        ) -> Result<ToolResult> {
            Ok(ToolResult::failure("unused", 0))
        }

        fn has_tool(&self, _tool: &str) -> bool {
            false
        }
    }

    struct EchoAgentExecutor;

    #[async_trait::async_trait]
    impl AgentExecutor for EchoAgentExecutor {
        async fn run_agent_job(
            &self,
            config: &AgentJobConfig,
            _channel: &str,
        ) -> Result<AgentRunResult> {
            Ok(AgentRunResult {
                response: format!("done: {}", config.task),
                ..AgentRunResult::default()
            })
        }
    }

    struct PendingAgentExecutor;

    #[async_trait::async_trait]
    impl AgentExecutor for PendingAgentExecutor {
        async fn run_agent_job(
            &self,
            _config: &AgentJobConfig,
            _channel: &str,
        ) -> Result<AgentRunResult> {
            let () = std::future::pending().await;
            Ok(AgentRunResult::default())
        }
    }

    fn agent_config(task: &str) -> AgentJobConfig {
        AgentJobConfig::new(
            task.to_string(),
            AgentProfile::Full,
            workforce_common::session::TurnPrincipal::default(),
        )
    }

    fn agent_manifest(task: &str) -> JobManifest {
        JobManifest::new(
            "test".to_string(),
            JobType::Agent,
            JobConfig::Agent(agent_config(task)),
            None,
            600,
        )
    }

    fn test_runner(tmp: &tempfile::TempDir) -> JobRunner {
        JobRunner::new(
            "test".to_string(),
            JobPersistence::new(tmp.path().to_path_buf()),
            None,
            None,
            None,
            None,
            Arc::new(StubToolExecutor),
            5,
        )
    }

    #[tokio::test]
    async fn agent_job_runs_to_completion() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = test_runner(&tmp);
        runner.set_agent_executor(Arc::new(EchoAgentExecutor));

        let job_id = runner.submit(agent_manifest("look into X")).await.unwrap();

        for _ in 0..100 {
            let res = runner.get_result(&job_id, None).await.unwrap();
            match res["status"].as_str().unwrap() {
                "completed" => {
                    assert_eq!(res["output"]["text"].as_str().unwrap(), "done: look into X");
                    return;
                }
                "failed" => panic!("agent job failed: {res}"),
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        panic!("agent job did not finish");
    }

    /// Who a tool call reached the executor as.
    struct WhoAmI;

    #[async_trait::async_trait]
    impl ToolExecutor for WhoAmI {
        async fn execute_tool(
            &self,
            _tool: &str,
            _params: serde_json::Value,
        ) -> Result<ToolResult> {
            Ok(ToolResult::success(
                serde_json::json!(crate::user_config::current_identity_id()),
                0,
            ))
        }

        fn has_tool(&self, _tool: &str) -> bool {
            true
        }
    }

    /// A suspension stops every job still going as the person, whatever its
    /// kind: an agent job they run as, and a tool job they submitted.
    #[tokio::test]
    async fn the_jobs_still_going_as_a_person_are_found_whatever_their_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = test_runner(&tmp);
        let ada = workforce_common::session::TurnPrincipal::run_as(Some("ada".into()));
        let tool = || {
            JobManifest::new(
                "test".to_string(),
                JobType::Tool,
                JobConfig::Tool(ToolJobConfig {
                    tool: "whoami".to_string(),
                    params: serde_json::json!({}),
                }),
                None,
                600,
            )
        };
        let mut agent = agent_manifest("hers");
        if let JobConfig::Agent(job) = &mut agent.config {
            job.user_id = Some("ada".into());
        }
        let submitted = crate::user_config::as_principal(&ada, async { tool() }).await;
        let mut finished = crate::user_config::as_principal(&ada, async { tool() }).await;
        finished.status = JobStatus::Completed;
        for manifest in [&agent, &submitted, &finished, &tool()] {
            runner
                .inner
                .persistence
                .save_manifest(manifest)
                .await
                .unwrap();
        }

        let mut found: Vec<String> = runner
            .active_run_as(&["ada".to_string()])
            .await
            .unwrap()
            .into_iter()
            .map(|job| job.job_id)
            .collect();
        found.sort();
        let mut expected = vec![agent.job_id.clone(), submitted.job_id.clone()];
        expected.sort();
        assert_eq!(found, expected);
    }

    /// The job runs in a task the submitting turn's identity does not reach,
    /// so it runs as the identity it recorded.
    #[tokio::test]
    async fn a_tool_job_runs_as_the_person_who_submitted_it() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = JobRunner::new(
            "test".to_string(),
            JobPersistence::new(tmp.path().to_path_buf()),
            None,
            None,
            None,
            None,
            Arc::new(WhoAmI),
            5,
        );
        let manifest = crate::user_config::as_principal(
            &workforce_common::session::TurnPrincipal::run_as(Some("ada".into())),
            async {
                JobManifest::new(
                    "test".to_string(),
                    JobType::Tool,
                    JobConfig::Tool(ToolJobConfig {
                        tool: "whoami".to_string(),
                        params: serde_json::json!({}),
                    }),
                    None,
                    600,
                )
            },
        )
        .await;
        let job_id = runner.submit(manifest).await.unwrap();

        for _ in 0..100 {
            let res = runner.get_result(&job_id, None).await.unwrap();
            match res["status"].as_str().unwrap() {
                "completed" => {
                    assert_eq!(res["output"]["tool_result"], serde_json::json!("ada"));
                    return;
                }
                "failed" => panic!("tool job failed: {res}"),
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        panic!("tool job did not finish");
    }

    /// Refuses `exec.run` to `member`, as the capability gate refuses a
    /// member without `ShellExec`.
    struct MembersMayNotShell;

    #[async_trait::async_trait]
    impl ToolExecutor for MembersMayNotShell {
        async fn execute_tool(
            &self,
            _tool: &str,
            _params: serde_json::Value,
        ) -> Result<ToolResult> {
            Ok(ToolResult::success(serde_json::Value::Null, 0))
        }

        fn has_tool(&self, _tool: &str) -> bool {
            true
        }

        fn refusal(
            &self,
            user: Option<&str>,
            tool: &str,
            _params: &serde_json::Value,
        ) -> Option<String> {
            (user == Some("member") && tool == "exec.run").then(|| "needs shell_exec".to_string())
        }
    }

    /// A job its submitter could not run is refused when it is submitted, so
    /// the turn hears why rather than finding a failed job later, and nothing
    /// of it is kept.
    #[tokio::test]
    async fn a_job_its_submitter_could_not_run_is_refused_at_submission() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = JobRunner::new(
            "test".to_string(),
            JobPersistence::new(tmp.path().to_path_buf()),
            None,
            None,
            None,
            None,
            Arc::new(MembersMayNotShell),
            5,
        );
        let shell = || {
            JobConfig::Shell(ShellJobConfig {
                command: "id".to_string(),
                workdir: None,
            })
        };
        let as_ =
            |who: &str| workforce_common::session::TurnPrincipal::run_as(Some(who.to_string()));
        let submit = |who: &'static str, config: JobConfig| {
            let runner = runner.clone();
            async move {
                crate::user_config::as_principal(&as_(who), async {
                    let manifest =
                        JobManifest::new("test".to_string(), JobType::Shell, config, None, 60);
                    runner.submit(manifest).await
                })
                .await
            }
        };
        let chain = |who: &'static str| {
            let runner = runner.clone();
            async move {
                let step = |id: &str, config| ChainStep {
                    id: id.to_string(),
                    config,
                    depends_on: Vec::new(),
                    label: None,
                    retry: None,
                    timeout_secs: None,
                    stream: false,
                };
                let tool = JobConfig::Tool(ToolJobConfig {
                    tool: "web.fetch".to_string(),
                    params: serde_json::json!({}),
                });
                let steps = vec![step("fetch", tool), step("build", shell())];
                let chain = runner.submit_chain(steps, "test".to_string(), None, None, None);
                crate::user_config::as_principal(&as_(who), chain).await
            }
        };

        let refused = submit("member", shell()).await.unwrap_err();
        assert_eq!(refused.to_string(), "needs shell_exec");
        let refused = chain("member").await.unwrap_err();
        assert_eq!(refused.to_string(), "Step 'build': needs shell_exec");
        assert!(
            runner.list(None, &JobReader::Admin).await.is_empty(),
            "nothing kept"
        );

        assert!(submit("admin", shell()).await.is_ok());
        assert!(chain("admin").await.is_ok());
    }

    #[tokio::test]
    async fn agent_job_without_executor_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = test_runner(&tmp);
        let err = runner.submit(agent_manifest("x")).await.unwrap_err();
        assert!(err.to_string().contains("Agent execution is not available"));
    }

    #[test]
    fn augment_agent_task_appends_upstream_findings_handles() {
        let upstream = serde_json::json!({
            "w0": { "tool_result": { "findings": { "artifact_id": "art_1" } } },
            "w1": { "tool_result": { "findings": { "artifact_id": "art_2" } } },
            "w2": { "tool_result": {} },
        });
        let out = augment_agent_task("Synthesise.", Some(&upstream));
        assert!(out.starts_with("Synthesise."));
        assert!(out.contains("art_1") && out.contains("art_2"));
        assert!(out.contains("data.query"));

        assert_eq!(augment_agent_task("x", None), "x");
        let bare = serde_json::json!({ "w0": { "tool_result": {} } });
        assert_eq!(augment_agent_task("x", Some(&bare)), "x");
    }

    #[tokio::test]
    async fn execute_agent_job_times_out() {
        let executor: Arc<dyn AgentExecutor> = Arc::new(PendingAgentExecutor);
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let err = execute_agent_job(
            Some(&executor),
            &agent_config("x"),
            "agent-job:t",
            None,
            rx,
            0,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn execute_agent_job_cancels() {
        let executor: Arc<dyn AgentExecutor> = Arc::new(PendingAgentExecutor);
        let (tx, rx) = tokio::sync::watch::channel(false);
        tx.send(true).unwrap();
        let err = execute_agent_job(
            Some(&executor),
            &agent_config("x"),
            "agent-job:t",
            None,
            rx,
            600,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("cancelled"));
    }

    #[tokio::test]
    async fn worker_job_completes_empty_instead_of_failing() {
        let executor: Arc<dyn AgentExecutor> = Arc::new(PendingAgentExecutor);
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let cfg = AgentJobConfig {
            profile: AgentProfile::Worker,
            ..agent_config("x")
        };
        // Timeout (0s) on a worker resolves to a completed-empty result, not an error,
        // so a stuck worker can't cascade-fail the investigation synthesiser.
        let (out, _) = execute_agent_job(Some(&executor), &cfg, "agent-job:t", None, rx, 0)
            .await
            .unwrap();
        assert!(out.full_output.contains("could not complete"));
        assert!(out.tool_result.is_none());
    }

    #[tokio::test]
    async fn execute_agent_job_requires_executor() {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let err = execute_agent_job(None, &agent_config("x"), "agent-job:t", None, rx, 600)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not available"));
    }

    #[test]
    fn agent_completion_message_previews_response_head() {
        let mut manifest = agent_manifest("look into X");
        manifest.status = JobStatus::Completed;
        let body = format!("HEAD-ANSWER\n{}TAIL-SIGNOFF", "filler\n".repeat(18));
        let output = JobOutput {
            job_id: manifest.job_id.clone(),
            tool_result: None,
            stdout: None,
            stderr: None,
            exit_code: None,
            full_output: body,
            attempt: None,
        };

        let msg = format_completion_message(&manifest, Some(&output));

        // The head of the response (the answer) must survive — `tail(10)` would drop it.
        assert!(
            msg.contains("HEAD-ANSWER"),
            "expected response head; got: {msg}"
        );
        assert!(msg.contains("agent"));
    }

    #[test]
    fn agent_config_serde_roundtrips_with_tag() {
        let cfg = JobConfig::Agent(AgentJobConfig {
            model: Some("haiku".into()),
            ..AgentJobConfig::new(
                "investigate".into(),
                AgentProfile::Worker,
                workforce_common::session::TurnPrincipal::run_as(Some("u1".into())),
            )
        });
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["type"], "agent");
        assert_eq!(json["task"], "investigate");
        assert_eq!(json["profile"], "worker");
        let back: JobConfig = serde_json::from_value(json).unwrap();
        assert!(matches!(back, JobConfig::Agent(a) if a.model.as_deref() == Some("haiku")));
    }

    struct SequenceExecutor {
        outputs: Vec<serde_json::Value>,
        idx: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ToolExecutor for SequenceExecutor {
        async fn execute_tool(
            &self,
            _tool: &str,
            _params: serde_json::Value,
        ) -> Result<ToolResult> {
            let i = self
                .idx
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                .min(self.outputs.len() - 1);
            Ok(ToolResult::success(self.outputs[i].clone(), 0))
        }

        fn has_tool(&self, _tool: &str) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        messages: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl NotificationSink for RecordingSink {
        async fn send_notification(
            &self,
            _channel: &str,
            _thread: Option<&str>,
            text: &str,
        ) -> Result<()> {
            self.messages.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn change_watch_notifies_on_watched_path_only_ignoring_volatile_fields() {
        let executor: Arc<dyn ToolExecutor> = Arc::new(SequenceExecutor {
            outputs: vec![
                serde_json::json!({ "status": "queued", "checked_at": 1 }),
                serde_json::json!({ "status": "queued", "checked_at": 2 }),
                serde_json::json!({ "status": "running", "checked_at": 3 }),
            ],
            idx: std::sync::atomic::AtomicUsize::new(0),
        });
        let sink = Arc::new(RecordingSink::default());
        let config = MonitorJobConfig {
            tool: "deploy_status".to_string(),
            params: serde_json::json!({}),
            interval_secs: 0,
            until: String::new(),
            max_polls: 3,
            polls_completed: 0,
            notify_on: NotifyOn::Change,
            watch_path: Some("status".to_string()),
        };
        let delivery = MonitorDelivery {
            sink: Some(sink.clone() as Arc<dyn NotificationSink>),
            channel: Some("C1".to_string()),
            thread: None,
        };
        let (_tx, rx) = tokio::sync::watch::channel(false);

        let (_out, polls) = execute_monitor_job(&executor, &config, delivery, rx, 3600)
            .await
            .unwrap();

        assert_eq!(polls, Some(3));
        let messages = sink.messages.lock().unwrap();
        // queued→running is one change; the moving `checked_at` is ignored.
        assert_eq!(messages.len(), 1, "got: {messages:?}");
        assert!(messages[0].contains("running"));
    }

    #[tokio::test]
    async fn condition_monitor_returns_when_condition_met() {
        let executor: Arc<dyn ToolExecutor> = Arc::new(SequenceExecutor {
            outputs: vec![
                serde_json::json!({ "status": "pending" }),
                serde_json::json!({ "status": "done" }),
            ],
            idx: std::sync::atomic::AtomicUsize::new(0),
        });
        let config = MonitorJobConfig {
            tool: "ci".to_string(),
            params: serde_json::json!({}),
            interval_secs: 0,
            until: "status == \"done\"".to_string(),
            max_polls: 5,
            polls_completed: 0,
            notify_on: NotifyOn::Condition,
            watch_path: None,
        };
        let delivery = MonitorDelivery {
            sink: None,
            channel: None,
            thread: None,
        };
        let (_tx, rx) = tokio::sync::watch::channel(false);

        let (out, polls) = execute_monitor_job(&executor, &config, delivery, rx, 3600)
            .await
            .unwrap();

        assert_eq!(polls, Some(2));
        assert_eq!(out.tool_result.unwrap()["status"], "done");
    }
}

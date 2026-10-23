//! Memory observability — periodic snapshots of in-process cache sizes.
//!
//! The hub holds several long-lived in-RAM data structures (session caches,
//! WASM module cache, orchestrator task map, job-runner state, etc.) that
//! grew unboundedly until this module was added. We can't fix what we
//! can't see — each snapshot writes the numbers to the `metrics` facade as
//! Prometheus gauges (the source of truth, scraped from `/api/metrics`) and
//! mirrors them to a structured `debug!` event (`event = "memory_metrics"`)
//! for local tailing.
//!
//! All log emissions use the `memory_metrics` target at `debug` level. The
//! baseline suppression in `workforce_common::logging` keeps this target out
//! of the stream even under a coarse `RUST_LOG=info`, so the per-tick snapshot
//! never drowns out other diagnostics; opt back in with
//! `RUST_LOG="...,memory_metrics=debug"`. Observability in prod comes from the
//! scraped gauges, not these logs.
//!
//! Labels stay process-wide (`source`, bounded `label`) — never include
//! session/task/user IDs (see the cardinality policy in
//! `workforce_common::logging`).
//!
//! Wiring: build a [`MemoryMetrics`] in `serve.rs` after the relevant
//! subsystems are constructed, register each source with `add_*`, then
//! call [`MemoryMetrics::spawn`] to kick off the background task.
//!
//! The collector clones `Arc`s into the background task, which means it
//! pins those subsystems for the process lifetime. That's intentional —
//! the components it watches are themselves process-lifetime, and `Weak`
//! handling adds complexity without buying anything in this hub.
//!
//! On Linux the snapshot also reads `/proc/self/status` for `VmRSS`,
//! which gives us the ground-truth resident-set size to correlate
//! against the per-cache numbers. On non-Linux dev hosts the RSS field
//! is omitted.

use std::sync::Arc;
use std::time::Duration;

use tracing::debug;

use workforce_common::session::SessionStore;
use workforce_knowledge_service::KnowledgeService;
use workforce_memory::store::MemoryStore;
use workforce_orchestrator::orchestrator::Orchestrator;
use workforce_wasm_runtime::engine::WasmSkillEngine;

use crate::hosts::JobHost;

/// Default snapshot interval. 60s is the sweet spot: short enough to
/// catch sustained growth before it hits the OOM ceiling, long enough
/// that the snapshot itself isn't a measurable cost.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

pub struct MemoryMetrics {
    interval: Duration,
    sessions: Vec<(String, Arc<dyn SessionStore>)>,
    wasm: Vec<(String, Arc<WasmSkillEngine>)>,
    orchestrator: Option<Arc<Orchestrator>>,
    job_runners: Vec<(String, Arc<dyn JobHost>)>,
    knowledge: Option<Arc<KnowledgeService>>,
    memory_store: Option<Arc<dyn MemoryStore>>,
    /// Whether to emit the process-wide RSS line. Disable on per-agent
    /// instances when a hub-level instance already logs it, to avoid
    /// N copies of the same number per tick when N agents share a hub.
    emit_rss: bool,
}

impl MemoryMetrics {
    pub fn new() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            sessions: Vec::new(),
            wasm: Vec::new(),
            orchestrator: None,
            job_runners: Vec::new(),
            knowledge: None,
            memory_store: None,
            emit_rss: true,
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    pub fn with_rss(mut self, emit: bool) -> Self {
        self.emit_rss = emit;
        self
    }

    pub fn add_session_store(&mut self, label: impl Into<String>, store: Arc<dyn SessionStore>) {
        self.sessions.push((label.into(), store));
    }

    pub fn add_wasm_engine(&mut self, label: impl Into<String>, engine: Arc<WasmSkillEngine>) {
        self.wasm.push((label.into(), engine));
    }

    pub fn set_orchestrator(&mut self, orch: Arc<Orchestrator>) {
        self.orchestrator = Some(orch);
    }

    pub fn add_job_runner(&mut self, label: impl Into<String>, runner: Arc<dyn JobHost>) {
        self.job_runners.push((label.into(), runner));
    }

    pub fn set_knowledge_service(&mut self, service: Arc<KnowledgeService>) {
        self.knowledge = Some(service);
    }

    pub fn set_memory_store(&mut self, store: Arc<dyn MemoryStore>) {
        self.memory_store = Some(store);
    }

    /// Consume self and spawn the periodic snapshot task. Returns immediately;
    /// the task runs until the runtime shuts down.
    pub fn spawn(self) {
        if self.sessions.is_empty()
            && self.wasm.is_empty()
            && self.orchestrator.is_none()
            && self.job_runners.is_empty()
            && self.knowledge.is_none()
            && self.memory_store.is_none()
        {
            tracing::warn!("MemoryMetrics has no registered sources; not spawning logger");
            return;
        }

        let interval = self.interval;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            // Skip the immediate first tick — gives the hub a chance to
            // finish startup before the first snapshot.
            tick.tick().await;
            loop {
                tick.tick().await;
                self.emit_snapshot().await;
            }
        });
    }

    async fn emit_snapshot(&self) {
        for (label, store) in &self.sessions {
            let s = store.cache_stats();
            debug!(
                target: "memory_metrics",
                event = "memory_metrics",
                source = "session_store",
                label = %label,
                sessions = s.sessions,
                messages = s.messages,
                estimated_bytes = s.estimated_bytes,
            );
            metrics::gauge!("workforce.session_store.sessions", "label" => label.clone())
                .set(s.sessions as f64);
            metrics::gauge!("workforce.session_store.messages", "label" => label.clone())
                .set(s.messages as f64);
            metrics::gauge!("workforce.session_store.estimated_bytes", "label" => label.clone())
                .set(s.estimated_bytes as f64);
        }
        for (label, engine) in &self.wasm {
            let s = engine.cache_stats().await;
            debug!(
                target: "memory_metrics",
                event = "memory_metrics",
                source = "wasm_engine",
                label = %label,
                modules = s.modules,
                source_bytes = s.source_bytes,
            );
            metrics::gauge!("workforce.wasm.modules", "label" => label.clone())
                .set(s.modules as f64);
            metrics::gauge!("workforce.wasm.source_bytes", "label" => label.clone())
                .set(s.source_bytes as f64);
        }
        if let Some(orch) = &self.orchestrator {
            let tasks = orch.task_count().await;
            let active = orch.active_assignment_count().await;
            debug!(
                target: "memory_metrics",
                event = "memory_metrics",
                source = "orchestrator",
                tasks = tasks,
                active_assignments = active,
            );
            metrics::gauge!("workforce.orchestrator.tasks").set(tasks as f64);
            metrics::gauge!("workforce.orchestrator.active_assignments").set(active as f64);
        }
        if let Some(svc) = &self.knowledge {
            let s = svc.cache_stats();
            debug!(
                target: "memory_metrics",
                event = "memory_metrics",
                source = "knowledge_service",
                db_bytes = s.db_bytes,
                indexing_repos = s.indexing_repos,
            );
            metrics::gauge!("workforce.knowledge.db_bytes").set(s.db_bytes as f64);
            metrics::gauge!("workforce.knowledge.indexing_repos").set(s.indexing_repos as f64);
        }
        if let Some(store) = &self.memory_store {
            let s = store.cache_stats();
            debug!(
                target: "memory_metrics",
                event = "memory_metrics",
                source = "memory_store",
                db_bytes = s.db_bytes,
            );
            metrics::gauge!("workforce.memory_store.db_bytes").set(s.db_bytes as f64);
        }
        for (label, runner) in &self.job_runners {
            let s = runner.cache_stats().await;
            debug!(
                target: "memory_metrics",
                event = "memory_metrics",
                source = "job_runner",
                label = %label,
                jobs = s.jobs,
                handles = s.handles,
                queue = s.queue,
                chains = s.chains,
                streamers = s.streamers,
            );
            metrics::gauge!("workforce.job_runner.jobs", "label" => label.clone())
                .set(s.jobs as f64);
            metrics::gauge!("workforce.job_runner.handles", "label" => label.clone())
                .set(s.handles as f64);
            metrics::gauge!("workforce.job_runner.queue", "label" => label.clone())
                .set(s.queue as f64);
            metrics::gauge!("workforce.job_runner.chains", "label" => label.clone())
                .set(s.chains as f64);
            metrics::gauge!("workforce.job_runner.streamers", "label" => label.clone())
                .set(s.streamers as f64);
        }
        if self.emit_rss {
            if let Some(s) = read_proc_status() {
                debug!(
                    target: "memory_metrics",
                    event = "memory_metrics",
                    source = "process",
                    rss_kib = s.rss_kib,
                    peak_vsize_kib = s.peak_vsize_kib,
                    hwm_kib = s.hwm_kib,
                    data_kib = s.data_kib,
                    rss_anon_kib = s.rss_anon_kib,
                );
                // Convert KiB to bytes for the canonical OTel/Prometheus
                // unit. Per-field gauges keep dashboards simple.
                metrics::gauge!("workforce.process.memory.rss_bytes")
                    .set((s.rss_kib * 1024) as f64);
                metrics::gauge!("workforce.process.memory.peak_vsize_bytes")
                    .set((s.peak_vsize_kib * 1024) as f64);
                metrics::gauge!("workforce.process.memory.hwm_bytes")
                    .set((s.hwm_kib * 1024) as f64);
                metrics::gauge!("workforce.process.memory.data_bytes")
                    .set((s.data_kib * 1024) as f64);
                metrics::gauge!("workforce.process.memory.rss_anon_bytes")
                    .set((s.rss_anon_kib * 1024) as f64);
            }
            if let Some(rt) = tokio_runtime_snapshot() {
                debug!(
                    target: "memory_metrics",
                    event = "memory_metrics",
                    source = "tokio_runtime",
                    workers = rt.workers,
                );
                metrics::gauge!("workforce.tokio.workers").set(rt.workers as f64);
            }
        }
    }
}

impl Default for MemoryMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of process-level memory fields from `/proc/self/status`.
///
/// `VmPeak` and `VmHWM` matter as much as `VmRSS` here — the +280 MB
/// spike we observed in driftwerk-prod happened between snapshots, so a
/// 60s sample of `VmRSS` alone misses it. `VmHWM` is high-water-mark
/// RSS and never decreases, so the diff between successive `VmHWM`
/// readings tells us "how much did we peak by" even when the live
/// `VmRSS` has already dropped back down.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcStatus {
    pub rss_kib: u64,
    /// `VmPeak` — peak virtual memory size since process start.
    pub peak_vsize_kib: u64,
    /// `VmHWM` — peak resident set size (high-water mark).
    pub hwm_kib: u64,
    /// `VmData` — data segment size (heap + global state).
    pub data_kib: u64,
    /// `RssAnon` — anonymous (non-file-backed) resident pages.
    /// Distinguishes "we're holding lots of mmap'd files" from "we're
    /// holding lots of allocated heap".
    pub rss_anon_kib: u64,
}

/// Read process memory fields from `/proc/self/status` on Linux.
/// Returns `None` on non-Linux platforms or when the file isn't readable.
#[cfg(target_os = "linux")]
fn read_proc_status() -> Option<ProcStatus> {
    let content = std::fs::read_to_string("/proc/self/status").ok()?;
    let mut s = ProcStatus::default();
    for line in content.lines() {
        let parse_kib = |rest: &str| -> u64 {
            rest.split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0)
        };
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            s.rss_kib = parse_kib(rest);
        } else if let Some(rest) = line.strip_prefix("VmPeak:") {
            s.peak_vsize_kib = parse_kib(rest);
        } else if let Some(rest) = line.strip_prefix("VmHWM:") {
            s.hwm_kib = parse_kib(rest);
        } else if let Some(rest) = line.strip_prefix("VmData:") {
            s.data_kib = parse_kib(rest);
        } else if let Some(rest) = line.strip_prefix("RssAnon:") {
            s.rss_anon_kib = parse_kib(rest);
        }
    }
    Some(s)
}

#[cfg(not(target_os = "linux"))]
fn read_proc_status() -> Option<ProcStatus> {
    None
}

#[derive(Debug, Default, Clone, Copy)]
struct TokioRuntimeSnapshot {
    workers: u64,
}

/// Sample the tokio runtime worker count. Each worker carries a thread
/// stack (~2 MiB on glibc) plus per-thread allocator caches, so the
/// number is a small but useful contributor to the RSS picture. Richer
/// metrics (blocking threads, alive task counts) need `tokio_unstable`,
/// which the workspace doesn't enable.
fn tokio_runtime_snapshot() -> Option<TokioRuntimeSnapshot> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let m = handle.metrics();
    Some(TokioRuntimeSnapshot {
        workers: m.num_workers() as u64,
    })
}

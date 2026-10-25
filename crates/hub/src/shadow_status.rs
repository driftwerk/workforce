//! What this hub process's shadows have done (DW-676, PR 9c), for
//! `/api/metrics`, `/api/health` and the admin status route: each read
//! kernel's counts and breaker, the comparisons memories' read shadow
//! dropped before its sink was open ([`Deferred`]), the sink's own counts,
//! and the memory mirror's backlog.
//!
//! Every count but the backlog is **this process's**: a deployment may run several
//! hub replicas, and each answers for itself. Datadog sums them by host;
//! the report's gates never rest on them (they read Postgres alone).
//!
//! The backlog is one Postgres table, so either replica gives the true
//! figure. It is asked in the async handler, under [`BACKLOG_LIMIT`]: a
//! backlog not read in time, or not readable, is told as unknown, never as
//! 0 or as the last figure read.
//!
//! [`Deferred`]: crate::memory_erasure::Deferred

use std::time::Duration;

use serde_json::json;
use workforce_common::settings::WorkforceConfig;
use workforce_common::shadow::ShadowCounts;
use workforce_common::shadow_postgres::{Backlog, SinkCounts};

/// How long a metrics scrape or a status read waits for the backlog.
pub const BACKLOG_LIMIT: Duration = Duration::from_secs(2);

/// One store's read kernel in this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelState {
    pub store: &'static str,
    /// Off while `[vellev.shadow] paused` (read at boot).
    pub enabled: bool,
    /// `closed`, `open` or `half_open`.
    pub breaker: &'static str,
    pub counts: ShadowCounts,
}

/// The memory mirror's noter in this process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoterState {
    pub written: u64,
    pub dropped: u64,
    pub lost: u64,
    /// 1 once its pending list could not be made ready.
    pub setup_failed: u64,
}

/// Everything this process counts of its shadows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessState {
    pub kernels: Vec<KernelState>,
    /// Memories' comparisons dropped for want of an open sink; `None` with
    /// no memory read shadow in this process.
    pub deferred_dropped: Option<u64>,
    /// The Postgres sink's counts, once it is open.
    pub sink: Option<SinkCounts>,
    pub noter: Option<NoterState>,
}

/// This process's shadows, as they stand.
pub fn process_state() -> ProcessState {
    use std::sync::atomic::Ordering;
    let kernel = |shadow: &workforce_common::shadow::Shadow, store| KernelState {
        store,
        enabled: shadow.enabled(),
        breaker: shadow.breaker_told(),
        counts: shadow.counts(),
    };
    let mut kernels = Vec::new();
    if let Some(shadow) = crate::memory_erasure::memory_read_kernel() {
        kernels.push(kernel(&shadow, "memories"));
    }
    if let Some(shadow) = crate::serve::artifact_shadow() {
        kernels.push(kernel(&shadow, "artifacts"));
    }
    ProcessState {
        kernels,
        deferred_dropped: crate::memory_erasure::deferred_dropped(),
        sink: crate::shadow::sink_counts(),
        noter: crate::memory_erasure::noter_counts().map(|c| NoterState {
            written: c.written.load(Ordering::Relaxed),
            dropped: c.dropped.load(Ordering::Relaxed),
            lost: c.lost.load(Ordering::Relaxed),
            setup_failed: c.setup_failed.load(Ordering::Relaxed),
        }),
    }
}

/// The memory mirror's backlog as last asked: read, or unknown and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BacklogSeen {
    Known(Backlog),
    Unknown(String),
}

/// `read` within `limit`: its backlog, or unknown when it fails or is not
/// done in time.
pub async fn await_backlog<F>(read: F, limit: Duration) -> BacklogSeen
where
    F: std::future::Future<Output = eyre::Result<Backlog>>,
{
    match tokio::time::timeout(limit, read).await {
        Ok(Ok(backlog)) => BacklogSeen::Known(backlog),
        Ok(Err(e)) => BacklogSeen::Unknown(format!("{e:#}")),
        Err(_) => BacklogSeen::Unknown(format!("not read within {limit:?}")),
    }
}

/// The memory mirror's backlog when memories are shadowed (`None`
/// otherwise), within [`BACKLOG_LIMIT`]. Read on the shadow tables' pool
/// the process already holds (the sink's and retention's), so a scrape
/// opens no connection of its own; when that pool is busy the read waits
/// at most [`BACKLOG_LIMIT`] and is told unknown.
pub async fn backlog(config: &WorkforceConfig) -> Option<BacklogSeen> {
    if config.memory.backend != "shadow" {
        return None;
    }
    let read = async {
        // Only the tables this process already opened: a scrape opens no
        // pool and migrates nothing.
        let tables = crate::shadow::opened_store()
            .ok_or_else(|| eyre::eyre!("the shadow tables are not open in this process"))?;
        tables.mirror_backlog().await
    };
    Some(await_backlog(read, BACKLOG_LIMIT).await)
}

fn line(out: &mut String, name: &str, kind: &str, samples: &[(String, u64)]) {
    out.push_str(&format!("# TYPE {name} {kind}\n"));
    for (labels, value) in samples {
        out.push_str(&format!("{name}{labels} {value}\n"));
    }
}

/// `state` and `backlog` as Prometheus exposition lines, each name under
/// the `workforce_` prefix the Datadog check filters on.
pub fn render_metrics(out: &mut String, state: &ProcessState, backlog: Option<&BacklogSeen>) {
    if !state.kernels.is_empty() {
        let mut counts = Vec::new();
        let mut enabled = Vec::new();
        let mut open = Vec::new();
        for k in &state.kernels {
            let c = &k.counts;
            for (name, value) in [
                ("offered", c.offered),
                ("paused", c.paused),
                ("unsampled", c.unsampled),
                ("no_runtime", c.no_runtime),
                ("busy", c.busy),
                ("breaker_open", c.breaker_open),
                ("started", c.started),
                ("ok", c.ok),
                ("failed", c.failed),
                ("panicked", c.panicked),
                ("timed_out", c.timed_out),
            ] {
                counts.push((format!("{{store=\"{}\",count=\"{name}\"}}", k.store), value));
            }
            let store = format!("{{store=\"{}\"}}", k.store);
            enabled.push((store.clone(), u64::from(k.enabled)));
            open.push((store, u64::from(k.breaker == "open")));
        }
        line(out, "workforce_shadow_kernel", "counter", &counts);
        line(out, "workforce_shadow_enabled", "gauge", &enabled);
        line(out, "workforce_shadow_breaker_open", "gauge", &open);
    }
    if let Some(dropped) = state.deferred_dropped {
        line(
            out,
            "workforce_shadow_memory_deferred_dropped",
            "counter",
            &[(String::new(), dropped)],
        );
    }
    if let Some(sink) = state.sink {
        let samples: Vec<(String, u64)> = [
            ("recorded", sink.recorded),
            ("dropped", sink.dropped),
            ("written", sink.written),
            ("failed", sink.failed),
        ]
        .into_iter()
        .map(|(name, v)| (format!("{{count=\"{name}\"}}"), v))
        .collect();
        line(out, "workforce_shadow_sink", "counter", &samples);
    }
    match backlog {
        None => {}
        Some(BacklogSeen::Known(b)) => {
            line(
                out,
                "workforce_shadow_memory_backlog_unknown",
                "gauge",
                &[(String::new(), 0)],
            );
            line(
                out,
                "workforce_shadow_memory_backlog_pending",
                "gauge",
                &[(String::new(), u64::try_from(b.pending).unwrap_or(0))],
            );
            let age = b
                .oldest
                .map(|at| (chrono::Utc::now() - at).num_seconds().max(0) as u64)
                .unwrap_or(0);
            line(
                out,
                "workforce_shadow_memory_backlog_oldest_secs",
                "gauge",
                &[(String::new(), age)],
            );
        }
        // Unknown is said, and no count is: a 0 here would read as "none".
        Some(BacklogSeen::Unknown(_)) => line(
            out,
            "workforce_shadow_memory_backlog_unknown",
            "gauge",
            &[(String::new(), 1)],
        ),
    }
}

/// The backlog as JSON: `"unknown"` with why, never 0, when not read.
pub fn backlog_json(backlog: Option<&BacklogSeen>) -> serde_json::Value {
    match backlog {
        None => serde_json::Value::Null,
        Some(BacklogSeen::Known(b)) => json!({"pending": b.pending, "oldest": b.oldest}),
        Some(BacklogSeen::Unknown(why)) => json!({"pending": "unknown", "why": why}),
    }
}

/// `state` as `/api/health`'s `shadow` block: counts only, naming no
/// memory, artifact or error, as health is read without a key. `null`
/// when this process shadows nothing.
pub fn health_json(state: &ProcessState, paused: bool) -> serde_json::Value {
    if state.kernels.is_empty()
        && state.deferred_dropped.is_none()
        && state.sink.is_none()
        && state.noter.is_none()
    {
        return serde_json::Value::Null;
    }
    let kernels: serde_json::Map<String, serde_json::Value> = state
        .kernels
        .iter()
        .map(|k| {
            let c = &k.counts;
            (
                k.store.to_string(),
                json!({
                    "enabled": k.enabled,
                    "breaker": k.breaker,
                    "offered": c.offered, "paused": c.paused, "unsampled": c.unsampled,
                    "no_runtime": c.no_runtime, "busy": c.busy,
                    "breaker_open": c.breaker_open, "started": c.started, "ok": c.ok,
                    "failed": c.failed, "panicked": c.panicked, "timed_out": c.timed_out,
                }),
            )
        })
        .collect();
    json!({
        // Every count here is this hub process's alone.
        "scope": "this hub process",
        // As read at boot: a change needs every replica restarted.
        "paused": paused,
        "kernels": kernels,
        "memory_deferred_dropped": state.deferred_dropped,
        "sink": state.sink.map(|s| json!({
            "recorded": s.recorded, "dropped": s.dropped, "written": s.written, "failed": s.failed,
        })),
        "memory_noter": state.noter.map(|n| json!({
            "written": n.written, "dropped": n.dropped, "lost": n.lost,
            "setup_failed": n.setup_failed,
        })),
    })
}

#[cfg(test)]
#[path = "shadow_status_tests.rs"]
mod tests;

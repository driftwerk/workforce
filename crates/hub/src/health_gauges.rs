//! Fleet-health gauges rendered directly into `/api/metrics`.
//!
//! These three 0/1 gauges exist so Datadog can page on "hub healthy but
//! nobody holds Slack ingress" — the failure mode every liveness probe is
//! blind to. They bypass the `metrics` recorder
//! on purpose: writes to an already-registered gauge key were empirically
//! swallowed by the Prometheus recorder in the full hub process (first
//! write lands, later writes freeze — reproduced only in the real serve
//! boot, not in reduced tests), and a health gauge that can't change value
//! is worse than none. Plain atomics read at render time cannot go stale.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// The leader-election flag, stashed at wiring time. Unset means no
/// election is running (single-instance deployment) — which renders as
/// leader=1, matching the "ungated singleton is the leader" semantics.
static LEADER_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Whether this instance currently holds the Slack ingress sockets.
pub static SLACK_INGRESS_UP: AtomicBool = AtomicBool::new(false);

/// Stash the election's leadership flag for rendering. Idempotent; only
/// the first call wins (there is one election per process).
pub fn set_leader_flag(flag: Arc<AtomicBool>) {
    let _ = LEADER_FLAG.set(flag);
}

pub fn is_leader() -> bool {
    LEADER_FLAG
        .get()
        .map(|f| f.load(Ordering::Relaxed))
        .unwrap_or(true)
}

/// Render the health gauges as Prometheus exposition lines. Appended to
/// the recorder's snapshot by the `/api/metrics` handler. Names must keep
/// the `workforce` prefix — the Datadog OpenMetrics check filters on it.
///
/// `backlog` is the memory mirror's, read by the async handler before this
/// is called (`None` when memories are not shadowed); shadow mode's
/// process counts are read here ([`crate::shadow_status`]).
pub fn render(
    nats_connected: Option<bool>,
    backlog: Option<&crate::shadow_status::BacklogSeen>,
) -> String {
    let mut out = String::new();
    let flag = |v: bool| if v { 1 } else { 0 };
    out.push_str("# TYPE workforce_hub_is_leader gauge\n");
    out.push_str(&format!("workforce_hub_is_leader {}\n", flag(is_leader())));
    out.push_str("# TYPE workforce_slack_ingress_up gauge\n");
    out.push_str(&format!(
        "workforce_slack_ingress_up {}\n",
        flag(SLACK_INGRESS_UP.load(Ordering::Relaxed))
    ));
    if let Some(connected) = nats_connected {
        out.push_str("# TYPE workforce_nats_connected gauge\n");
        out.push_str(&format!("workforce_nats_connected {}\n", flag(connected)));
    }
    if let Some(counts) = crate::memory_erasure::noter_counts() {
        render_noter(&mut out, &counts);
    }
    crate::shadow_status::render_metrics(&mut out, &crate::shadow_status::process_state(), backlog);
    out
}

/// The memory mirror's noter (DW-676): memories written and noted for
/// vellev's replica, dropped (no room, not resolved, or no noter), or lost
/// (Postgres failed), and whether the noter could not be made ready.
/// Dropped and lost are found by the next reconcile.
fn render_noter(out: &mut String, counts: &crate::memory_mirror::NoteCounts) {
    for (name, counter) in [
        ("workforce_shadow_memory_notes_written", &counts.written),
        ("workforce_shadow_memory_notes_dropped", &counts.dropped),
        ("workforce_shadow_memory_notes_lost", &counts.lost),
    ] {
        out.push_str(&format!("# TYPE {name} counter\n"));
        out.push_str(&format!("{name} {}\n", counter.load(Ordering::Relaxed)));
    }
    // 1 once the noter could not be made ready: nothing is noted while the
    // process runs.
    out.push_str("# TYPE workforce_shadow_memory_noter_failed gauge\n");
    out.push_str(&format!(
        "workforce_shadow_memory_noter_failed {}\n",
        counts.setup_failed.load(Ordering::Relaxed)
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_three_when_nats_configured() {
        SLACK_INGRESS_UP.store(true, Ordering::Relaxed);
        let out = render(Some(true), None);
        assert!(out.contains("workforce_hub_is_leader 1\n"));
        assert!(out.contains("workforce_slack_ingress_up 1\n"));
        assert!(out.contains("workforce_nats_connected 1\n"));
    }

    #[test]
    fn renders_the_memory_noter_s_counts() {
        let counts = crate::memory_mirror::NoteCounts::default();
        counts.dropped.store(3, Ordering::Relaxed);
        let mut out = String::new();
        render_noter(&mut out, &counts);
        assert!(
            out.contains("workforce_shadow_memory_notes_dropped 3\n"),
            "{out}"
        );
        assert!(
            out.contains("workforce_shadow_memory_notes_lost 0\n"),
            "{out}"
        );
        counts.setup_failed.store(1, Ordering::Relaxed);
        let mut out = String::new();
        render_noter(&mut out, &counts);
        assert!(
            out.contains("workforce_shadow_memory_noter_failed 1\n"),
            "{out}"
        );
    }

    /// Shadow mode's lines reach `/api/metrics` through here: the backlog
    /// the handler read, told unknown when it could not.
    #[test]
    fn renders_shadow_mode_s_lines() {
        let unknown = crate::shadow_status::BacklogSeen::Unknown("timed out".into());
        let out = render(None, Some(&unknown));
        assert!(
            out.contains("workforce_shadow_memory_backlog_unknown 1\n"),
            "{out}"
        );
        assert!(!out.contains("backlog_pending"), "{out}");
    }

    #[test]
    fn omits_nats_when_not_configured() {
        let out = render(None, None);
        assert!(!out.contains("workforce_nats_connected"));
        assert!(out.contains("workforce_hub_is_leader"));
    }
}

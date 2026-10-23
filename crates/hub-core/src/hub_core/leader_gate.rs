//! Late-bound leadership gate for background singletons.
//!
//! The election (and its `Arc<AtomicBool>` flag) is wired in `main.rs` after
//! the agent services — and their background loops — are already built, so
//! the loops hold this gate and consult it at each tick instead of capturing
//! the flag at spawn time. Ungated (no election wired) means single-instance:
//! every singleton runs here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How long a boot-time singleton waits before consulting the gate in a
/// clustered deployment: long enough for the election to settle and for a
/// start-first deploy's outgoing instance to finish draining (its stop grace
/// is 150s), so a one-shot like job recovery never acts on work that is still
/// live over there. Matches the interrupted-session resume sweep's default.
pub const BOOT_SINGLETON_DELAY: Duration = Duration::from_secs(180);

#[derive(Default)]
pub struct LeaderGate {
    flag: parking_lot::RwLock<Option<Arc<AtomicBool>>>,
}

impl LeaderGate {
    /// Wire the election's leadership flag. Before this is called (or if it
    /// never is), the gate reports leadership unconditionally.
    pub fn set_flag(&self, flag: Arc<AtomicBool>) {
        *self.flag.write() = Some(flag);
    }

    /// Whether this instance should run process singletons right now.
    pub fn is_leader(&self) -> bool {
        self.flag
            .read()
            .as_ref()
            .map(|f| f.load(Ordering::Relaxed))
            .unwrap_or(true)
    }

    /// Whether an election is wired at all — i.e. whether peers may exist.
    pub fn is_clustered(&self) -> bool {
        self.flag.read().is_some()
    }

    /// Gate for boot-time one-shots: in a clustered deployment, wait out
    /// [`BOOT_SINGLETON_DELAY`] and then only proceed on the leader.
    ///
    /// The election flag is late-bound from `main.rs` *after* the services
    /// (and their one-shots) are spawned, so this first waits a moment before
    /// reading `is_clustered` — otherwise a clustered instance would see the
    /// still-unwired gate and run ungated.
    pub async fn wait_boot_singleton(&self) -> bool {
        tokio::time::sleep(Duration::from_secs(15)).await;
        if !self.is_clustered() {
            return true;
        }
        tokio::time::sleep(BOOT_SINGLETON_DELAY).await;
        self.is_leader()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwired_gate_is_always_leader() {
        let gate = LeaderGate::default();
        assert!(gate.is_leader());
        assert!(!gate.is_clustered());
    }

    #[test]
    fn wired_gate_tracks_the_flag() {
        let gate = LeaderGate::default();
        let flag = Arc::new(AtomicBool::new(false));
        gate.set_flag(Arc::clone(&flag));
        assert!(gate.is_clustered());
        assert!(!gate.is_leader());
        flag.store(true, Ordering::Relaxed);
        assert!(gate.is_leader());
    }

    #[tokio::test(start_paused = true)]
    async fn boot_singleton_waits_for_late_wiring() {
        let gate = Arc::new(LeaderGate::default());
        let waiter = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.wait_boot_singleton().await })
        };
        // The flag is wired after spawn — inside the settle window — as a
        // follower; the one-shot must see it and refuse to run.
        tokio::time::sleep(Duration::from_secs(1)).await;
        gate.set_flag(Arc::new(AtomicBool::new(false)));
        tokio::time::sleep(Duration::from_secs(15) + BOOT_SINGLETON_DELAY).await;
        assert!(!waiter.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn boot_singleton_runs_unclustered_after_settle() {
        let gate = Arc::new(LeaderGate::default());
        assert!(gate.wait_boot_singleton().await);
    }
}

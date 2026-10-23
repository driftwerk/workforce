//! Process-wide shutdown state.
//!
//! Set once when the drain sequence begins. Turn teardown paths consult it to
//! distinguish a user-initiated stop (end the session's turn record — the user
//! chose to abandon the turn) from a shutdown-initiated cancel (leave the
//! record — the successor instance's resume sweep re-runs the turn from the
//! saved transcript).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::AbortHandle;

use crate::agent_registry::AgentRegistry;

static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

pub fn begin() {
    SHUTTING_DOWN.store(true, Ordering::Relaxed);
}

pub fn in_progress() -> bool {
    SHUTTING_DOWN.load(Ordering::Relaxed)
}

/// How long the drain waits for ended detached work to unwind.
const DETACHED_GRACE: Duration = Duration::from_secs(5);

/// Work that runs outside an agent runner, such as a `/cc` turn or a fan-out
/// wave, whose rounds a successor collects once this process lets go of them.
static DETACHED: LazyLock<DetachedWork> = LazyLock::new(DetachedWork::default);

#[derive(Default)]
struct DetachedWork {
    tasks: Mutex<HashMap<u64, AbortHandle>>,
    next: AtomicU64,
    gone: Notify,
}

impl DetachedWork {
    fn tasks(&self) -> std::sync::MutexGuard<'_, HashMap<u64, AbortHandle>> {
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn register(&'static self, task: AbortHandle) -> Detached {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.tasks().insert(id, task);
        Detached { work: self, id }
    }

    /// End every registered task and wait, up to `grace`, for each to let go
    /// of its guard, so that none still runs once claims are released. Their
    /// rounds' jobs keep running: with the drain begun, a round let go of
    /// leaves its job for the successor.
    async fn end(&self, grace: Duration) -> usize {
        let ended = {
            let tasks = self.tasks();
            tasks.values().for_each(AbortHandle::abort);
            tasks.len()
        };
        let unwound = async {
            loop {
                let mut gone = std::pin::pin!(self.gone.notified());
                gone.as_mut().enable();
                if self.tasks().is_empty() {
                    return;
                }
                gone.await;
            }
        };
        if tokio::time::timeout(grace, unwound).await.is_err() {
            tracing::warn!(
                left = self.tasks().len(),
                "detached work still unwinding as the drain releases its claims"
            );
        }
        ended
    }
}

/// A task registered to be ended before the drain releases its claims, for
/// as long as this is held.
pub struct Detached {
    work: &'static DetachedWork,
    id: u64,
}

impl Detached {
    pub fn register(task: AbortHandle) -> Self {
        DETACHED.register(task)
    }
}

impl Drop for Detached {
    fn drop(&mut self) {
        self.work.tasks().remove(&self.id);
        self.work.gone.notify_waiters();
    }
}

/// Finish the drain for turns that outlived the natural wait: cancel them
/// cooperatively, give them `grace` to unwind their teardown (which, with
/// [`in_progress`] set, leaves their sessions marked for resume), abort any
/// that still haven't, end the detached work, then bulk-release every turn
/// claim this process owns. The bulk release is the load-bearing guarantee —
/// no claim outlives the process no matter how its turn died — and nothing
/// that could still post or release after it is left running.
pub async fn finish_drain(registry: &AgentRegistry, grace: Duration) -> (usize, u64) {
    let services = registry.all_services();

    let mut cancelled = 0usize;
    for svc in &services {
        for handle in svc.active_runners.read().await.values() {
            handle.cancel_token.cancel();
            cancelled += 1;
        }
    }
    if cancelled > 0 {
        tokio::time::sleep(grace).await;
        for svc in &services {
            for handle in svc.active_runners.read().await.values() {
                handle.abort();
            }
        }
    }

    let cancelled = cancelled + DETACHED.end(DETACHED_GRACE).await;
    (cancelled, release_owned_claims(registry))
}

/// Bulk-delete every turn claim owned by this process. Idempotent; called
/// once inside [`finish_drain`] and once more at the very end of shutdown to
/// catch a renewal upsert that was already in flight when the drain began.
pub fn release_owned_claims(registry: &AgentRegistry) -> u64 {
    let mut released = 0u64;
    for svc in registry.all_services() {
        match svc.sessions.release_owned_turn_claims() {
            Ok(n) => released += n,
            Err(e) => {
                tracing::warn!(error = %e, agent = %svc.agent_id, "Releasing owned turn claims failed")
            }
        }
    }
    released
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detached_work_is_ended_and_waited_for_once_the_drain_lets_go() {
        let work: &'static DetachedWork = Box::leak(Box::default());
        let (unwound_tx, unwound) = tokio::sync::oneshot::channel();
        let running = tokio::spawn(std::future::pending::<()>());
        let guard = work.register(running.abort_handle());
        let owner = tokio::spawn(async move {
            let _ = running.await;
            drop(guard);
            let _ = unwound_tx.send(());
        });
        drop(work.register(tokio::spawn(async {}).abort_handle()));

        assert_eq!(work.end(Duration::from_secs(5)).await, 1);
        assert!(work.tasks().is_empty(), "the drain waited for it to unwind");
        unwound.await.unwrap();
        owner.await.unwrap();
        assert_eq!(work.end(Duration::from_secs(5)).await, 0);
    }
}

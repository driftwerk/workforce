//! The hub's bounds on subscription CLI work, each its own pool so a call
//! that holds one never waits on the same one again.

use std::future::Future;
use std::sync::Arc;

use futures::FutureExt;
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::{CancellationToken, DropGuard};
use workforce_common::settings::SubscriptionConfig;

/// A permit wait its caller cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cancelled while waiting for a subscription permit")]
pub struct Cancelled;

/// A bound on concurrent work; `None` inside is unbounded.
#[derive(Clone)]
pub struct Pool(Option<Arc<Semaphore>>);

impl Pool {
    /// A pool of `max` permits; `0` is unbounded.
    pub fn new(max: u32) -> Self {
        Self((max > 0).then(|| Arc::new(Semaphore::new(max as usize))))
    }

    /// A permit, held until dropped; `None` when the pool is unbounded.
    pub async fn acquire(
        &self,
        cancel: Option<&CancellationToken>,
    ) -> Result<Option<OwnedSemaphorePermit>, Cancelled> {
        crate::hub_core::cc_budget::acquire_cc_permit(self.0.clone(), cancel)
            .await
            .map_err(|()| Cancelled)
    }
}

/// The install's subscription pools (`[subscription]`, restart-only).
pub struct SubscriptionPools {
    completions: Pool,
    turns: Pool,
}

impl SubscriptionPools {
    pub fn new(config: &SubscriptionConfig) -> Self {
        Self {
            completions: Pool::new(config.max_concurrent_completions),
            turns: Pool::new(config.max_concurrent_cli_turns),
        }
    }

    /// One-shot CLI completions.
    pub fn completions(&self) -> &Pool {
        &self.completions
    }

    /// Agent turns on a CLI route.
    pub fn turns(&self) -> &Pool {
        &self.turns
    }
}

/// A CLI turn's permit of the [`SubscriptionPools::turns`] pool, given back
/// while the turn waits on anything outside itself — a tool call or a
/// person's approval — and taken again before the wait's result returns. A
/// turn holding a permit therefore never waits on work that needs one, so a
/// call that starts a nested CLI turn cannot deadlock the pool.
#[derive(Clone)]
pub struct TurnSlot(Arc<SlotState>);

struct SlotState {
    pool: Pool,
    /// Ends a wait to take the permit back: the turn is over.
    ended: CancellationToken,
    held: Mutex<Parking>,
}

/// A turn's [`TurnSlot`] for as long as the turn runs: dropped, it ends any
/// wait to take the permit back, so none outlives the turn.
pub struct HeldSlot {
    slot: TurnSlot,
    _turn: DropGuard,
}

impl HeldSlot {
    pub fn slot(&self) -> &TurnSlot {
        &self.slot
    }
}

/// How many waits are in flight, and the permit while none is.
struct Parking {
    waits: usize,
    permit: Option<OwnedSemaphorePermit>,
}

impl TurnSlot {
    /// A permit of `pool` for a turn that `cancel` ends; `on_wait` runs
    /// once, when none is free at once.
    pub async fn acquire(
        pool: &Pool,
        cancel: &CancellationToken,
        on_wait: impl Future<Output = ()>,
    ) -> Result<HeldSlot, Cancelled> {
        let permit = match pool.acquire(Some(cancel)).now_or_never() {
            Some(permit) => permit?,
            None => {
                on_wait.await;
                pool.acquire(Some(cancel)).await?
            }
        };
        let ended = cancel.child_token();
        Ok(HeldSlot {
            slot: Self(Arc::new(SlotState {
                pool: pool.clone(),
                ended: ended.clone(),
                held: Mutex::new(Parking { waits: 0, permit }),
            })),
            _turn: ended.drop_guard(),
        })
    }

    /// `wait`, run with the permit given back. Waits in flight at once give
    /// it back once and take it again once, after the last of them.
    pub async fn parked<F: Future>(&self, wait: F) -> F::Output {
        let parked = Parked::begin(self);
        let output = wait.await;
        parked.end().await;
        output
    }

    fn park(&self) {
        let mut held = self.0.held.lock();
        held.waits += 1;
        held.permit = None;
    }

    /// Whether the last wait just ended, so the permit is to be taken back.
    fn unpark(&self) -> bool {
        let mut held = self.0.held.lock();
        held.waits -= 1;
        held.waits == 0
    }

    async fn reclaim(&self) {
        let Ok(permit) = self.0.pool.acquire(Some(&self.0.ended)).await else {
            return;
        };
        let mut held = self.0.held.lock();
        if held.waits == 0 && held.permit.is_none() {
            held.permit = permit;
        }
    }

    /// Whether the turn holds its permit now.
    #[cfg(any(test, feature = "test-support"))]
    pub fn holds(&self) -> bool {
        let held = self.0.held.lock();
        held.waits == 0 && (held.permit.is_some() || self.0.pool.0.is_none())
    }
}

/// One wait of a [`TurnSlot`]. Dropped before it ends — its wait abandoned —
/// it still takes the permit back, in a task of its own.
struct Parked {
    slot: Option<TurnSlot>,
}

impl Parked {
    fn begin(slot: &TurnSlot) -> Self {
        slot.park();
        Self {
            slot: Some(slot.clone()),
        }
    }

    async fn end(mut self) {
        if let Some(slot) = self.slot.take() {
            if slot.unpark() {
                slot.reclaim().await;
            }
        }
    }
}

impl Drop for Parked {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            if slot.unpark() {
                tokio::spawn(async move { slot.reclaim().await });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn abandoned_wait_stops_taking_the_permit_back_once_the_turn_ends() {
        let pool = Pool::new(1);
        let held = TurnSlot::acquire(&pool, &CancellationToken::new(), async {})
            .await
            .unwrap();
        let mut wait = Box::pin(held.slot().parked(std::future::pending::<()>()));
        assert!(futures::poll!(&mut wait).is_pending());
        let other = pool.acquire(None).await.unwrap();
        drop(wait);
        let state = Arc::downgrade(&held.slot().0);

        drop(held);

        tokio::time::timeout(Duration::from_secs(5), async {
            while state.upgrade().is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the wait to take the permit back ends with the turn");
        drop(other);
    }

    #[tokio::test]
    async fn completion_pool_bounds_and_zero_is_unbounded() {
        let pools = SubscriptionPools::new(&SubscriptionConfig {
            max_concurrent_completions: 2,
            ..Default::default()
        });
        let pool = pools.completions();
        let first = pool.acquire(None).await.unwrap();
        let second = pool.acquire(None).await.unwrap();
        assert!(first.is_some() && second.is_some());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), pool.acquire(None))
                .await
                .is_err(),
            "a third call waits for a permit"
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(pool.acquire(Some(&cancel)).await.err(), Some(Cancelled));
        drop(first);
        assert!(pool.acquire(None).await.unwrap().is_some());

        let unbounded = Pool::new(0);
        let held: Vec<_> = futures::future::join_all((0..64).map(|_| unbounded.acquire(None)))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert!(held.iter().all(Option::is_none), "0 takes no permit");
        assert_eq!(
            SubscriptionPools::new(&SubscriptionConfig::default())
                .completions()
                .0
                .as_ref()
                .map(|sem| sem.available_permits()),
            Some(4)
        );
    }
}

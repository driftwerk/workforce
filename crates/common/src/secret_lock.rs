//! Serialises read-modify-write cycles on a secret (OAuth refresh-token
//! rotation). A [`crate::settings::SecretStore`] picks the reach of its lock:
//! [`LocalRefreshLock`] covers one process, stores shared between processes
//! return a lock those processes share.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Weak};

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

/// Held for the duration of a refresh; dropping it releases the lock.
pub struct RefreshGuard {
    _inner: Box<dyn Send + Sync>,
}

impl RefreshGuard {
    pub fn new<T: Send + Sync + 'static>(inner: T) -> Self {
        Self {
            _inner: Box::new(inner),
        }
    }
}

#[async_trait]
pub trait RefreshLock: Send + Sync {
    /// Acquire the lock for `key`, waiting for its current holder.
    async fn lock(&self, key: &str) -> eyre::Result<RefreshGuard>;
}

/// Process-wide keyed lock: callers with the same key take turns, callers
/// with different keys never wait on each other.
#[derive(Default)]
pub struct LocalRefreshLock {
    keys: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
}

impl LocalRefreshLock {
    /// The instance every store in this process shares.
    pub fn shared() -> Arc<dyn RefreshLock> {
        static SHARED: LazyLock<Arc<LocalRefreshLock>> = LazyLock::new(Arc::default);
        SHARED.clone()
    }

    fn mutex_for(&self, key: &str) -> Arc<AsyncMutex<()>> {
        let mut keys = self.keys.lock();
        keys.retain(|_, mutex| mutex.strong_count() > 0);
        if let Some(mutex) = keys.get(key).and_then(Weak::upgrade) {
            return mutex;
        }
        let mutex = Arc::new(AsyncMutex::new(()));
        keys.insert(key.to_owned(), Arc::downgrade(&mutex));
        mutex
    }
}

#[async_trait]
impl RefreshLock for LocalRefreshLock {
    async fn lock(&self, key: &str) -> eyre::Result<RefreshGuard> {
        Ok(RefreshGuard::new(self.mutex_for(key).lock_owned().await))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn same_key_callers_take_turns() {
        let lock = Arc::new(LocalRefreshLock::default());
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..16)
            .map(|_| {
                let lock = Arc::clone(&lock);
                let in_flight = Arc::clone(&in_flight);
                let max_seen = Arc::clone(&max_seen);
                tokio::spawn(async move {
                    let _guard = lock.lock("token").await.unwrap();
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for handle in handles {
            handle.await.unwrap();
        }
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn different_keys_do_not_wait_on_each_other() {
        let lock = LocalRefreshLock::default();
        let _held = lock.lock("a").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), lock.lock("b"))
            .await
            .expect("an unrelated key must not block")
            .unwrap();
    }

    #[tokio::test]
    async fn released_keys_are_forgotten() {
        let lock = LocalRefreshLock::default();
        drop(lock.lock("a").await.unwrap());
        let _next = lock.lock("b").await.unwrap();
        assert_eq!(lock.keys.lock().len(), 1);
    }

    #[tokio::test]
    async fn shared_instance_is_process_wide() {
        let first = LocalRefreshLock::shared();
        let _held = first.lock("shared-instance-test").await.unwrap();
        let second = LocalRefreshLock::shared();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                second.lock("shared-instance-test")
            )
            .await
            .is_err(),
            "a second handle must contend on the same key"
        );
    }
}

//! What was found out a little while ago, kept for a while so it isn't
//! asked again on every request.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// Values by key, each kept for `ttl` from when it was put.
pub(crate) struct Recent<K, V> {
    ttl: Duration,
    kept: parking_lot::Mutex<HashMap<K, (Instant, V)>>,
}

impl<K: Eq + Hash, V: Clone> Recent<K, V> {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            kept: Default::default(),
        }
    }

    /// The value kept for `key`, while it is fresh.
    pub(crate) fn get(&self, key: &K) -> Option<V> {
        let now = Instant::now();
        self.fresh(now).get(key).map(|(_, v)| v.clone())
    }

    pub(crate) fn put(&self, key: K, value: V) {
        let now = Instant::now();
        self.fresh(now).insert(key, (now, value));
    }

    /// Whether nothing fresh is kept for `key`; if so it is kept now, so
    /// requests at once see it once.
    pub(crate) fn due(&self, key: K, value: V) -> bool {
        let now = Instant::now();
        let mut kept = self.fresh(now);
        match kept.entry(key) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((now, value));
                true
            }
        }
    }

    fn fresh(&self, now: Instant) -> parking_lot::MutexGuard<'_, HashMap<K, (Instant, V)>> {
        let mut kept = self.kept.lock();
        kept.retain(|_, (at, _)| now.duration_since(*at) < self.ttl);
        kept
    }

    #[cfg(test)]
    pub(crate) fn age(&self, by: Duration) {
        self.kept.lock().values_mut().for_each(|(at, _)| *at -= by);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_is_kept_until_it_is_stale() {
        let recent = Recent::new(Duration::from_secs(60));
        assert!(recent.due("sam", 0));
        assert!(!recent.due("sam", 0));
        assert!(recent.due("kim", 0));
        recent.put("pat", 7);
        assert_eq!(recent.get(&"pat"), Some(7));
        recent.age(Duration::from_secs(60));
        assert_eq!(recent.get(&"pat"), None);
        assert!(recent.due("sam", 0));
    }
}

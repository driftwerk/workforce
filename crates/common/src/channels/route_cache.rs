//! Bounded LRU + TTL cache mapping `(platform, channel_id)` → `workspace_id`.
//!
//! Populated by successful surface probes; lets repeat calls skip the
//! probe round-trip. Entries older than `ttl` are treated as misses; we
//! never serve a positively cached route past its TTL. Negative results
//! are deliberately not cached — channel membership can change outside
//! our control (operator `/invite`, bot kick, channel rename) and a
//! single fresh probe is preferable to a stuck negative.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lru::LruCache;
use tokio::sync::Mutex;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct RouteKey {
    platform: String,
    channel: String,
}

#[derive(Clone, Debug)]
struct RouteEntry {
    workspace_id: String,
    inserted_at: Instant,
}

#[derive(Clone)]
pub struct ChannelRouteCache {
    inner: Arc<Mutex<LruCache<RouteKey, RouteEntry>>>,
    ttl: Duration,
}

impl ChannelRouteCache {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("capacity ≥ 1");
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(cap))),
            ttl,
        }
    }

    pub async fn lookup(&self, platform: &str, channel: &str) -> Option<String> {
        let key = RouteKey {
            platform: platform.to_string(),
            channel: channel.to_string(),
        };
        let mut guard = self.inner.lock().await;
        let entry = guard.get(&key)?.clone();
        if entry.inserted_at.elapsed() <= self.ttl {
            Some(entry.workspace_id)
        } else {
            guard.pop(&key);
            None
        }
    }

    pub async fn insert(&self, platform: &str, channel: &str, workspace_id: &str) {
        let key = RouteKey {
            platform: platform.to_string(),
            channel: channel.to_string(),
        };
        let entry = RouteEntry {
            workspace_id: workspace_id.to_string(),
            inserted_at: Instant::now(),
        };
        self.inner.lock().await.put(key, entry);
    }

    pub async fn invalidate(&self, platform: &str, channel: &str) {
        let key = RouteKey {
            platform: platform.to_string(),
            channel: channel.to_string(),
        };
        self.inner.lock().await.pop(&key);
    }

    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }
}

impl Default for ChannelRouteCache {
    fn default() -> Self {
        Self::new(1024, Duration::from_secs(3600))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn insert_then_lookup_hits() {
        let cache = ChannelRouteCache::new(8, Duration::from_secs(60));
        cache.insert("slack", "C1", "candy").await;
        assert_eq!(cache.lookup("slack", "C1").await.as_deref(), Some("candy"));
    }

    #[tokio::test]
    async fn lookup_misses_on_unknown_key() {
        let cache = ChannelRouteCache::new(8, Duration::from_secs(60));
        assert!(cache.lookup("slack", "C1").await.is_none());
    }

    #[tokio::test]
    async fn lookup_after_ttl_is_miss() {
        let cache = ChannelRouteCache::new(8, Duration::from_millis(20));
        cache.insert("slack", "C1", "candy").await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(cache.lookup("slack", "C1").await.is_none());
        assert_eq!(cache.len().await, 0, "expired entry must be evicted");
    }

    #[tokio::test]
    async fn invalidate_removes_entry() {
        let cache = ChannelRouteCache::new(8, Duration::from_secs(60));
        cache.insert("slack", "C1", "candy").await;
        cache.invalidate("slack", "C1").await;
        assert!(cache.lookup("slack", "C1").await.is_none());
    }

    #[tokio::test]
    async fn re_insert_updates_workspace() {
        let cache = ChannelRouteCache::new(8, Duration::from_secs(60));
        cache.insert("slack", "C1", "candy").await;
        cache.insert("slack", "C1", "atlas").await;
        assert_eq!(cache.lookup("slack", "C1").await.as_deref(), Some("atlas"));
    }

    #[tokio::test]
    async fn platform_namespacing_is_respected() {
        let cache = ChannelRouteCache::new(8, Duration::from_secs(60));
        cache.insert("slack", "C1", "candy").await;
        cache.insert("discord", "C1", "guild").await;
        assert_eq!(cache.lookup("slack", "C1").await.as_deref(), Some("candy"));
        assert_eq!(
            cache.lookup("discord", "C1").await.as_deref(),
            Some("guild")
        );
    }

    #[tokio::test]
    async fn capacity_evicts_oldest() {
        let cache = ChannelRouteCache::new(2, Duration::from_secs(60));
        cache.insert("slack", "C1", "w1").await;
        cache.insert("slack", "C2", "w2").await;
        cache.insert("slack", "C3", "w3").await;
        assert!(cache.lookup("slack", "C1").await.is_none());
        assert_eq!(cache.lookup("slack", "C2").await.as_deref(), Some("w2"));
        assert_eq!(cache.lookup("slack", "C3").await.as_deref(), Some("w3"));
    }

    #[tokio::test]
    async fn capacity_zero_clamps_to_one() {
        let cache = ChannelRouteCache::new(0, Duration::from_secs(60));
        cache.insert("slack", "C1", "w1").await;
        assert_eq!(cache.lookup("slack", "C1").await.as_deref(), Some("w1"));
    }

    #[tokio::test]
    async fn lookup_promotes_to_most_recently_used() {
        let cache = ChannelRouteCache::new(2, Duration::from_secs(60));
        cache.insert("slack", "C1", "w1").await;
        cache.insert("slack", "C2", "w2").await;
        assert_eq!(cache.lookup("slack", "C1").await.as_deref(), Some("w1"));
        cache.insert("slack", "C3", "w3").await;
        assert!(cache.lookup("slack", "C2").await.is_none());
        assert_eq!(cache.lookup("slack", "C1").await.as_deref(), Some("w1"));
    }
}

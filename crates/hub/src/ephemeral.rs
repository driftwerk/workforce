//! A per-identity, age-bounded registry for state an MCP client may come
//! back for later: chat turns that outlived their wait and tool calls parked
//! on an approval. Bounded so an abandoned client cannot pin memory.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

/// Something a caller owns for a while.
pub trait Ephemeral: Send + Sync + 'static {
    fn owner(&self) -> &str;
    fn started(&self) -> Instant;
    /// Runs when the entry is dropped for age or capacity, never on an
    /// explicit `remove`.
    fn evict(&self) {}
}

pub struct EphemeralRegistry<T> {
    entries: DashMap<String, Arc<T>>,
    per_identity: usize,
    ttl: Duration,
}

impl<T: Ephemeral> EphemeralRegistry<T> {
    pub fn bounded(per_identity: usize, ttl: Duration) -> Self {
        Self {
            entries: DashMap::new(),
            per_identity,
            ttl,
        }
    }

    /// Register an entry under a fresh id. Expired entries are swept first;
    /// if the owner is at their cap the oldest is evicted to make room.
    pub fn insert(&self, entry: Arc<T>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.insert_as(id.clone(), entry);
        id
    }

    /// Register an entry under a caller-chosen id (an approval request id).
    pub fn insert_as(&self, id: String, entry: Arc<T>) {
        self.sweep(Instant::now());
        let mut owned: Vec<(String, Instant)> = self
            .entries
            .iter()
            .filter(|e| e.value().owner() == entry.owner())
            .map(|e| (e.key().clone(), e.value().started()))
            .collect();
        owned.sort_by_key(|(_, started)| *started);
        for (id, _) in owned.iter().take(
            owned
                .len()
                .saturating_sub(self.per_identity.saturating_sub(1)),
        ) {
            self.evict(id);
        }
        self.entries.insert(id, entry);
    }

    pub fn get(&self, id: &str, owner: &str) -> Option<Arc<T>> {
        let now = Instant::now();
        self.entries
            .get(id)
            .map(|e| Arc::clone(e.value()))
            .filter(|t| t.owner() == owner && self.live(t, now))
    }

    /// The owner's first live entry matching `pick`, with its id.
    pub fn find(&self, owner: &str, pick: impl Fn(&T) -> bool) -> Option<(String, Arc<T>)> {
        let now = Instant::now();
        self.entries
            .iter()
            .filter(|e| e.value().owner() == owner && self.live(e.value(), now) && pick(e.value()))
            .map(|e| (e.key().clone(), Arc::clone(e.value())))
            .next()
    }

    /// Claim the owner's live entry: whoever removes it acts on it, so two
    /// racing callers cannot both proceed.
    pub fn take(&self, id: &str, owner: &str) -> Option<Arc<T>> {
        let now = Instant::now();
        self.entries
            .remove_if(id, |_, t| t.owner() == owner && self.live(t, now))
            .map(|(_, entry)| entry)
    }

    pub fn remove(&self, id: &str) {
        self.entries.remove(id);
    }

    fn live(&self, entry: &T, now: Instant) -> bool {
        now.duration_since(entry.started()) <= self.ttl
    }

    fn evict(&self, id: &str) {
        if let Some((_, entry)) = self.entries.remove(id) {
            entry.evict();
        }
    }

    fn sweep(&self, now: Instant) {
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|e| !self.live(e.value(), now))
            .map(|e| e.key().clone())
            .collect();
        expired.iter().for_each(|id| self.evict(id));
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Note {
        owner: &'static str,
        started: Instant,
    }

    impl Ephemeral for Note {
        fn owner(&self) -> &str {
            self.owner
        }

        fn started(&self) -> Instant {
            self.started
        }
    }

    fn note(owner: &'static str, age: Duration) -> Arc<Note> {
        Arc::new(Note {
            owner,
            started: Instant::now().checked_sub(age).unwrap(),
        })
    }

    #[test]
    fn expired_entries_are_invisible_and_taken_only_once() {
        let registry = EphemeralRegistry::bounded(4, Duration::from_secs(60));
        let fresh = registry.insert(note("alice", Duration::ZERO));
        let stale = registry.insert(note("alice", Duration::from_secs(61)));
        assert!(registry.get(&fresh, "alice").is_some());
        assert!(registry.get(&stale, "alice").is_none(), "past its ttl");
        assert!(registry
            .find("alice", |_| true)
            .is_some_and(|(id, _)| id == fresh));
        assert!(registry.take(&fresh, "bob").is_none(), "owner-scoped");
        assert!(registry.take(&fresh, "alice").is_some());
        assert!(registry.take(&fresh, "alice").is_none(), "claimed once");
    }
}

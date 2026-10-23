use parking_lot::RwLock;
use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Advisory file lock manager for multi-agent conflict prevention.
///
/// Locks are advisory — agents should check before writing, but the
/// system won't block writes. This prevents accidental conflicts
/// while maintaining flexibility.
pub struct LockManager {
    locks: RwLock<HashMap<String, FileLock>>,
}

/// An advisory lock on a file path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileLock {
    /// The locked path.
    pub path: String,
    /// Who holds the lock.
    pub holder: String,
    /// When the lock was acquired.
    pub acquired_at: DateTime<Utc>,
    /// When the lock expires (auto-release).
    pub expires_at: DateTime<Utc>,
    /// Reason for the lock.
    pub reason: Option<String>,
}

/// Result of trying to acquire a lock.
#[derive(Debug, Clone)]
pub enum LockResult {
    /// Lock acquired successfully.
    Acquired,
    /// Lock is held by another agent.
    Held(FileLock),
    /// Lock was expired and has been taken over.
    TakenOver { previous_holder: String },
}

impl LockManager {
    pub fn new() -> Self {
        Self {
            locks: RwLock::new(HashMap::new()),
        }
    }

    /// Try to acquire a lock on a path.
    pub fn acquire(
        &self,
        path: &str,
        holder: &str,
        ttl_secs: u64,
        reason: Option<&str>,
    ) -> LockResult {
        let mut locks = self.locks.write();
        let now = Utc::now();

        if let Some(existing) = locks.get(path) {
            if existing.holder == holder {
                // Already held by us — extend
                let lock = FileLock {
                    path: path.to_string(),
                    holder: holder.to_string(),
                    acquired_at: existing.acquired_at,
                    expires_at: now + chrono::Duration::seconds(ttl_secs as i64),
                    reason: reason.map(String::from).or(existing.reason.clone()),
                };
                locks.insert(path.to_string(), lock);
                return LockResult::Acquired;
            }

            if existing.expires_at > now {
                // Held by someone else, not expired
                return LockResult::Held(existing.clone());
            }

            // Expired — take over
            let previous = existing.holder.clone();
            let lock = FileLock {
                path: path.to_string(),
                holder: holder.to_string(),
                acquired_at: now,
                expires_at: now + chrono::Duration::seconds(ttl_secs as i64),
                reason: reason.map(String::from),
            };
            locks.insert(path.to_string(), lock);
            return LockResult::TakenOver {
                previous_holder: previous,
            };
        }

        // Not locked — acquire
        let lock = FileLock {
            path: path.to_string(),
            holder: holder.to_string(),
            acquired_at: now,
            expires_at: now + chrono::Duration::seconds(ttl_secs as i64),
            reason: reason.map(String::from),
        };
        locks.insert(path.to_string(), lock);
        LockResult::Acquired
    }

    /// Release a lock. Only the holder can release.
    pub fn release(&self, path: &str, holder: &str) -> bool {
        let mut locks = self.locks.write();
        if let Some(lock) = locks.get(path) {
            if lock.holder == holder {
                locks.remove(path);
                return true;
            }
        }
        false
    }

    /// Check if a path is locked (and by whom).
    pub fn check(&self, path: &str) -> Option<FileLock> {
        let locks = self.locks.read();
        let now = Utc::now();
        locks
            .get(path)
            .filter(|lock| lock.expires_at > now)
            .cloned()
    }

    /// List all active (non-expired) locks.
    pub fn active_locks(&self) -> Vec<FileLock> {
        let locks = self.locks.read();
        let now = Utc::now();
        locks
            .values()
            .filter(|lock| lock.expires_at > now)
            .cloned()
            .collect()
    }

    /// Clean up expired locks.
    pub fn cleanup_expired(&self) -> usize {
        let mut locks = self.locks.write();
        let now = Utc::now();
        let before = locks.len();
        locks.retain(|_, lock| lock.expires_at > now);
        before - locks.len()
    }

    /// Get lock count (including expired).
    pub fn count(&self) -> usize {
        self.locks.read().len()
    }
}

impl Default for LockManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_and_release() {
        let mgr = LockManager::new();
        let result = mgr.acquire("src/main.rs", "dev-1", 300, Some("editing"));
        assert!(matches!(result, LockResult::Acquired));

        assert!(mgr.check("src/main.rs").is_some());

        let released = mgr.release("src/main.rs", "dev-1");
        assert!(released);
        assert!(mgr.check("src/main.rs").is_none());
    }

    #[test]
    fn conflict_detection() {
        let mgr = LockManager::new();
        mgr.acquire("src/lib.rs", "dev-1", 300, None);

        let result = mgr.acquire("src/lib.rs", "dev-2", 300, None);
        assert!(matches!(result, LockResult::Held(_)));
    }

    #[test]
    fn same_holder_extends() {
        let mgr = LockManager::new();
        mgr.acquire("file.rs", "dev-1", 300, None);

        let result = mgr.acquire("file.rs", "dev-1", 600, None);
        assert!(matches!(result, LockResult::Acquired));
    }

    #[test]
    fn expired_lock_takeover() {
        let mgr = LockManager::new();
        // Create an already-expired lock
        {
            let mut locks = mgr.locks.write();
            locks.insert(
                "old.rs".to_string(),
                FileLock {
                    path: "old.rs".to_string(),
                    holder: "dev-1".to_string(),
                    acquired_at: Utc::now() - chrono::Duration::hours(1),
                    expires_at: Utc::now() - chrono::Duration::minutes(1),
                    reason: None,
                },
            );
        }

        let result = mgr.acquire("old.rs", "dev-2", 300, None);
        assert!(matches!(result, LockResult::TakenOver { .. }));
    }

    #[test]
    fn wrong_holder_cannot_release() {
        let mgr = LockManager::new();
        mgr.acquire("file.rs", "dev-1", 300, None);

        let released = mgr.release("file.rs", "dev-2");
        assert!(!released);
        assert!(mgr.check("file.rs").is_some());
    }

    #[test]
    fn cleanup_expired() {
        let mgr = LockManager::new();
        mgr.acquire("active.rs", "dev-1", 300, None);

        // Add expired lock
        {
            let mut locks = mgr.locks.write();
            locks.insert(
                "expired.rs".to_string(),
                FileLock {
                    path: "expired.rs".to_string(),
                    holder: "dev-1".to_string(),
                    acquired_at: Utc::now() - chrono::Duration::hours(1),
                    expires_at: Utc::now() - chrono::Duration::minutes(1),
                    reason: None,
                },
            );
        }

        assert_eq!(mgr.count(), 2);
        let cleaned = mgr.cleanup_expired();
        assert_eq!(cleaned, 1);
        assert_eq!(mgr.count(), 1);
    }

    #[test]
    fn active_locks_excludes_expired() {
        let mgr = LockManager::new();
        mgr.acquire("active.rs", "dev-1", 300, None);

        {
            let mut locks = mgr.locks.write();
            locks.insert(
                "expired.rs".to_string(),
                FileLock {
                    path: "expired.rs".to_string(),
                    holder: "dev-1".to_string(),
                    acquired_at: Utc::now() - chrono::Duration::hours(1),
                    expires_at: Utc::now() - chrono::Duration::minutes(1),
                    reason: None,
                },
            );
        }

        assert_eq!(mgr.active_locks().len(), 1);
    }
}

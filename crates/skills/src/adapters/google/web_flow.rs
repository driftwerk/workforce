//! In-memory state store for the hub-hosted Google OAuth web-redirect flow.
//!
//! Created once at hub startup, referenced by both:
//! - `GoogleSetupTool` (to insert a new entry at flow initiation), and
//! - the `/api/auth/google/callback` handler (to remove it on completion).
//!
//! Entries carry the PKCE verifier, the initiating user's id, and the chosen
//! workspace — the user id lets the callback write tokens at the right
//! per-user-scoped path without needing a `CURRENT_USER_ID` task-local (which
//! is not set in axum request handlers).
//!
//! State is ephemeral. On hub restart, any pending flow's state is lost and
//! the user must re-initiate via `google.setup(action='login')`. The TTL
//! defends against the unbounded-growth case where a user abandons a flow.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Time a pending web-flow state entry remains valid after `start_web_flow`.
pub const STATE_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct WebFlowEntry {
    pub uid: String,
    pub workspace: String,
    pub verifier: String,
    pub expires_at: Instant,
}

#[derive(Default)]
pub struct WebFlowStateStore {
    entries: Mutex<HashMap<String, WebFlowEntry>>,
}

impl WebFlowStateStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a pending flow, keyed by the random `state` parameter the user
    /// will carry through the redirect. Also prunes expired entries
    /// opportunistically — no background task required.
    pub fn insert(&self, state: String, uid: String, workspace: String, verifier: String) {
        let now = Instant::now();
        let mut map = self.entries.lock().expect("web_flow state mutex poisoned");
        map.retain(|_, e| e.expires_at > now);
        map.insert(
            state,
            WebFlowEntry {
                uid,
                workspace,
                verifier,
                expires_at: now + STATE_TTL,
            },
        );
    }

    /// Remove and return the entry for `state`, if one exists and hasn't
    /// expired. Single-use — a second call with the same state returns `None`
    /// even if the TTL has not passed.
    pub fn take(&self, state: &str) -> Option<WebFlowEntry> {
        let mut map = self.entries.lock().expect("web_flow state mutex poisoned");
        let entry = map.remove(state)?;
        (entry.expires_at > Instant::now()).then_some(entry)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("web_flow state mutex poisoned")
            .len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_returns_inserted_entry_once() {
        let store = WebFlowStateStore::new();
        store.insert("s1".into(), "kings".into(), "default".into(), "v1".into());

        let first = store.take("s1").expect("entry should be present");
        assert_eq!(first.uid, "kings");
        assert_eq!(first.workspace, "default");
        assert_eq!(first.verifier, "v1");

        assert!(store.take("s1").is_none(), "second take must return None");
    }

    #[test]
    fn take_returns_none_for_unknown_state() {
        let store = WebFlowStateStore::new();
        store.insert("s1".into(), "kings".into(), "default".into(), "v1".into());
        assert!(store.take("other").is_none());
        assert_eq!(store.len(), 1, "unknown take must not evict other entries");
    }

    #[test]
    fn insert_prunes_expired_entries() {
        let store = WebFlowStateStore::new();
        // Seed an already-expired entry by hand.
        store
            .entries
            .lock()
            .expect("web_flow state mutex poisoned")
            .insert(
                "stale".into(),
                WebFlowEntry {
                    uid: "kings".into(),
                    workspace: "default".into(),
                    verifier: "v".into(),
                    expires_at: Instant::now() - Duration::from_secs(1),
                },
            );
        assert_eq!(store.len(), 1);

        store.insert(
            "fresh".into(),
            "kings".into(),
            "default".into(),
            "v2".into(),
        );
        assert_eq!(store.len(), 1, "stale entry should have been pruned");
        assert!(store.take("stale").is_none());
        assert!(store.take("fresh").is_some());
    }
}

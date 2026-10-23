//! In-memory per-session citation history.
//!
//! Every time `compose_prompt` runs the training recall, the resulting
//! chunk set is captured here as a `TurnCitations` entry keyed by
//! session. The `training_citations` agent tool reads from this
//! ring buffer when the user asks "what did you cite?" — it returns
//! the *previous* turn's recall, since the current turn's recall is
//! almost certainly empty (the user just asked a meta question).
//!
//! Storage is in-memory only: citation history is conversational state,
//! not durable. Per-session capacity is bounded so a long session
//! doesn't accumulate unbounded snapshots.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tokio::sync::RwLock;

/// One chunk that surfaced in a single turn's recall.
#[derive(Debug, Clone, PartialEq)]
pub struct Citation {
    pub short_id: String,
    pub filename: String,
    pub corpus: String,
    pub locator: String,
    pub score: f32,
    pub snippet: String,
}

/// All citations from a single recall pass (one user turn).
pub type TurnCitations = Vec<Citation>;

/// Max history entries kept per session. Five turns is enough for the
/// agent to look back across a short clarification dialogue without
/// growing memory for long-running threads.
pub const MAX_TURNS_PER_SESSION: usize = 5;

/// Max distinct sessions tracked at once. The per-session queue is bounded
/// (`MAX_TURNS_PER_SESSION`), but the *key set* was not — one entry was added
/// per distinct session and never removed, so on a long-lived hub it grew with
/// traffic. Over capacity we evict the least-recently-written session. 1024
/// covers far more concurrent threads than any real install while capping the
/// store at a few tens of MB worst case.
pub const MAX_SESSIONS: usize = 1024;

/// Snippet length for `Citation.snippet`. Long enough to recognise the
/// chunk, short enough to keep tool-result payloads small.
pub const SNIPPET_CHARS: usize = 200;

/// One session's citation history plus a monotonic write tick used for
/// least-recently-written eviction across sessions.
struct SessionEntry {
    turns: VecDeque<TurnCitations>,
    last_touch: u64,
}

/// All tracked sessions. `tick` is a process-monotonic counter stamped onto an
/// entry's `last_touch` on every push, so the smallest `last_touch` identifies
/// the session that has gone longest without a write.
#[derive(Default)]
struct Sessions {
    map: HashMap<String, SessionEntry>,
    tick: u64,
}

#[derive(Clone, Default)]
pub struct SessionCitationStore {
    inner: Arc<RwLock<Sessions>>,
}

impl SessionCitationStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one turn's recall for `session_key`. The oldest *turn* is evicted
    /// when the per-session queue exceeds capacity; the least-recently-written
    /// *session* is evicted when the key set exceeds [`MAX_SESSIONS`]. An active
    /// session re-stamps `last_touch` on every push, so only sessions that have
    /// stopped writing are reclaimed.
    pub async fn push(&self, session_key: impl Into<String>, citations: TurnCitations) {
        let mut guard = self.inner.write().await;
        guard.tick += 1;
        let tick = guard.tick;

        let entry = guard
            .map
            .entry(session_key.into())
            .or_insert_with(|| SessionEntry {
                turns: VecDeque::new(),
                last_touch: tick,
            });
        entry.last_touch = tick;
        entry.turns.push_back(citations);
        while entry.turns.len() > MAX_TURNS_PER_SESSION {
            entry.turns.pop_front();
        }

        if guard.map.len() > MAX_SESSIONS {
            if let Some(victim) = guard
                .map
                .iter()
                .min_by_key(|(_, e)| e.last_touch)
                .map(|(k, _)| k.clone())
            {
                guard.map.remove(&victim);
            }
        }
    }

    /// Return the *previous* turn's citations — the last completed
    /// turn before the current one started. `None` when fewer than
    /// two turns have been recorded for this session.
    pub async fn previous_turn(&self, session_key: &str) -> Option<TurnCitations> {
        let guard = self.inner.read().await;
        let entry = guard.map.get(session_key)?;
        if entry.turns.len() < 2 {
            return None;
        }
        entry.turns.get(entry.turns.len() - 2).cloned()
    }
}

/// Truncate the chunk body to `SNIPPET_CHARS` graphemes, appending an
/// ellipsis when the cut actually shortens the input. Single-line by
/// design: tool-result snippets render in chat without breaking flow.
pub fn make_snippet(body: &str) -> String {
    let flat: String = body
        .chars()
        .map(|c| if c.is_control() || c == '\n' { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if flat.chars().count() <= SNIPPET_CHARS {
        return flat;
    }
    let kept: String = flat.chars().take(SNIPPET_CHARS - 1).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cit(short_id: &str, score: f32) -> Citation {
        Citation {
            short_id: short_id.into(),
            filename: format!("{short_id}.pdf"),
            corpus: "x".into(),
            locator: "p.1".into(),
            score,
            snippet: "hello".into(),
        }
    }

    #[tokio::test]
    async fn previous_turn_returns_none_until_two_pushes() {
        let s = SessionCitationStore::new();
        assert!(s.previous_turn("k").await.is_none());
        s.push("k", vec![cit("a", 0.9)]).await;
        assert!(s.previous_turn("k").await.is_none());
        s.push("k", vec![cit("b", 0.8)]).await;
        let prev = s.previous_turn("k").await.unwrap();
        assert_eq!(prev[0].short_id, "a");
    }

    #[tokio::test]
    async fn ring_buffer_evicts_oldest_at_capacity() {
        let s = SessionCitationStore::new();
        for i in 0..(MAX_TURNS_PER_SESSION + 3) {
            s.push("k", vec![cit(&format!("c{i}"), 0.5)]).await;
        }
        // previous_turn returns the second-to-last entry; the buffer
        // length should be exactly MAX_TURNS_PER_SESSION.
        let prev = s.previous_turn("k").await.unwrap();
        let expected = MAX_TURNS_PER_SESSION + 3 - 2;
        assert_eq!(prev[0].short_id, format!("c{expected}"));
    }

    #[tokio::test]
    async fn sessions_are_isolated() {
        let s = SessionCitationStore::new();
        s.push("a", vec![cit("a1", 0.5)]).await;
        s.push("a", vec![cit("a2", 0.5)]).await;
        s.push("b", vec![cit("b1", 0.5)]).await;
        assert_eq!(s.previous_turn("a").await.unwrap()[0].short_id, "a1");
        assert!(s.previous_turn("b").await.is_none());
    }

    #[tokio::test]
    async fn evicts_least_recently_written_session_over_capacity() {
        let s = SessionCitationStore::new();
        // Fill to capacity, two pushes each so `previous_turn` is populated.
        for i in 0..MAX_SESSIONS {
            let k = format!("s{i}");
            s.push(&k, vec![cit("a", 0.5)]).await;
            s.push(&k, vec![cit("b", 0.5)]).await;
        }
        // Keep s1 warm; s0 is now the least-recently-written.
        s.push("s1", vec![cit("c", 0.5)]).await;
        // One more distinct session tips us over capacity.
        s.push("overflow", vec![cit("x", 0.5)]).await;
        s.push("overflow", vec![cit("y", 0.5)]).await;

        // The cold session was evicted; the warm one and the newcomer survive.
        assert!(
            s.previous_turn("s0").await.is_none(),
            "cold LRU session should be evicted"
        );
        assert!(
            s.previous_turn("s1").await.is_some(),
            "recently-written session should survive"
        );
        assert!(
            s.previous_turn("overflow").await.is_some(),
            "new session should survive"
        );
    }

    #[tokio::test]
    async fn previous_turn_returns_empty_when_last_turn_had_no_citations() {
        let s = SessionCitationStore::new();
        s.push("k", vec![cit("a", 0.9)]).await;
        s.push("k", vec![]).await;
        // Current turn was empty; the prior turn (entry at len-2) is
        // the one with `a`.
        let prev = s.previous_turn("k").await.unwrap();
        assert_eq!(prev.len(), 1);
        assert_eq!(prev[0].short_id, "a");
    }

    #[test]
    fn snippet_collapses_whitespace_and_truncates() {
        let long = "a".repeat(500);
        let s = make_snippet(&long);
        assert_eq!(s.chars().count(), SNIPPET_CHARS);
        assert!(s.ends_with('…'));

        assert_eq!(make_snippet("hello\n\nworld"), "hello world");
        assert_eq!(make_snippet("  x  y  "), "x y");
    }
}

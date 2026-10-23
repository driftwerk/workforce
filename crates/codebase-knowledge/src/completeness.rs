//! Computed view of indexing completeness.
//!
//! Status is derived from the data, not from a stored flag. Every field
//! on [`IndexCompleteness`] is the result of a count or a comparison
//! against the actual graph and lease state. No "summaries_complete"
//! boolean exists in the schema — there's nothing to keep in sync, so
//! nothing can drift.
//!
//! This module also holds [`NextAction`], the decision returned to
//! `trigger` callers. The full decision matrix:
//!
//! | Heartbeat fresh | `head_commit` vs `indexed_commit` | Summaries  | Action               |
//! |-----------------|-----------------------------------|------------|----------------------|
//! | fresh           | any                               | any        | `AlreadyIndexing`    |
//! | none / stale    | equal                             | all there  | `Complete`           |
//! | none / stale    | equal                             | gap        | `ResumeSummaries`    |
//! | none / stale    | differ, entities exist            | any        | `Incremental`        |
//! | none / stale    | no indexed commit yet             | any        | `Full`               |

use serde::{Deserialize, Serialize};

use crate::lease::IndexLease;

/// Fully-computed snapshot of an index for one repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexCompleteness {
    pub repo_id: String,
    pub head_commit: Option<String>,
    pub indexed_commit: Option<String>,
    pub last_full_index_at: Option<String>,
    pub last_incremental_at: Option<String>,
    pub entities: u64,
    pub vectors: u64,
    pub summaries_expected: u64,
    pub summaries_present: u64,
    pub summaries_stale: u64,
    pub generate_summaries: bool,
    pub lease: Option<IndexLease>,
}

impl IndexCompleteness {
    /// Number of summaries that should exist but don't (or are stale).
    pub fn summaries_missing(&self) -> u64 {
        let fresh = self.summaries_present.saturating_sub(self.summaries_stale);
        self.summaries_expected.saturating_sub(fresh)
    }

    /// True when the index has nothing left to do — entities are at HEAD
    /// and (if the policy enables them) every expected summary is fresh.
    pub fn is_current(&self) -> bool {
        let Some(head) = &self.head_commit else {
            return false;
        };
        let Some(indexed) = &self.indexed_commit else {
            return false;
        };
        head == indexed
            && self.entities > 0
            && (!self.generate_summaries || self.summaries_missing() == 0)
    }

    /// What `trigger` should do given this snapshot. Returns
    /// [`NextAction::AlreadyIndexing`] only when a fresh lease is held;
    /// stale leases are treated as "not running" so the caller can
    /// steal and resume.
    pub fn next_action(&self, now_ms: i64) -> NextAction {
        if let Some(l) = self.lease.as_ref().filter(|l| l.is_fresh(now_ms)) {
            return NextAction::AlreadyIndexing(l.clone());
        }
        if self.is_current() {
            return NextAction::Complete;
        }
        let head_at_indexed = matches!(
            (&self.head_commit, &self.indexed_commit),
            (Some(h), Some(i)) if h == i,
        );
        if head_at_indexed && self.entities > 0 && self.summaries_missing() > 0 {
            return NextAction::ResumeSummaries;
        }
        if self.entities > 0 && self.indexed_commit.is_some() {
            NextAction::Incremental
        } else {
            NextAction::Full
        }
    }
}

/// What `trigger` should do for a repo, given a fresh
/// [`IndexCompleteness`] snapshot.
#[derive(Debug, Clone)]
pub enum NextAction {
    /// Nothing to do — the index is at HEAD with all expected summaries.
    Complete,
    /// A fresh lease is held by some worker. Don't start a new run.
    AlreadyIndexing(IndexLease),
    /// Entities are at HEAD but summaries have a gap; run the summary
    /// phase only, picking up exactly the missing/stale entries.
    ResumeSummaries,
    /// HEAD has moved since the last index; run an incremental pass.
    Incremental,
    /// No prior index (or partial entities) — run from scratch.
    Full,
}

impl NextAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::AlreadyIndexing(_) => "already_indexing",
            Self::ResumeSummaries => "resume_summaries",
            Self::Incremental => "incremental",
            Self::Full => "full",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::{now_ms, IndexPhase, STALE_THRESHOLD};

    fn base() -> IndexCompleteness {
        IndexCompleteness {
            repo_id: "r".into(),
            head_commit: Some("abc".into()),
            indexed_commit: Some("abc".into()),
            last_full_index_at: None,
            last_incremental_at: None,
            entities: 100,
            vectors: 100,
            summaries_expected: 50,
            summaries_present: 50,
            summaries_stale: 0,
            generate_summaries: true,
            lease: None,
        }
    }

    fn fresh_lease() -> IndexLease {
        IndexLease {
            repo_id: "r".into(),
            worker_id: "w".into(),
            target_commit: Some("abc".into()),
            phase: IndexPhase::GeneratingSummaries,
            acquired_at_ms: now_ms(),
            last_heartbeat_at_ms: now_ms(),
            entities_total: Some(100),
            entities_processed: Some(100),
            summaries_total: Some(50),
            summaries_processed: Some(20),
        }
    }

    #[test]
    fn complete_when_everything_matches() {
        assert!(matches!(base().next_action(now_ms()), NextAction::Complete));
    }

    #[test]
    fn fresh_lease_blocks_other_actions() {
        let mut s = base();
        s.lease = Some(fresh_lease());
        assert!(matches!(
            s.next_action(now_ms()),
            NextAction::AlreadyIndexing(_)
        ));
    }

    #[test]
    fn stale_lease_does_not_block() {
        let mut s = base();
        let mut l = fresh_lease();
        l.last_heartbeat_at_ms = now_ms() - (STALE_THRESHOLD.as_millis() as i64) - 1_000;
        s.lease = Some(l);
        assert!(matches!(s.next_action(now_ms()), NextAction::Complete));
    }

    #[test]
    fn missing_summaries_at_head_resume() {
        let mut s = base();
        s.summaries_present = 30;
        assert_eq!(s.summaries_missing(), 20);
        assert!(matches!(
            s.next_action(now_ms()),
            NextAction::ResumeSummaries
        ));
    }

    #[test]
    fn stale_summaries_at_head_resume() {
        let mut s = base();
        s.summaries_stale = 10;
        assert_eq!(s.summaries_missing(), 10);
        assert!(matches!(
            s.next_action(now_ms()),
            NextAction::ResumeSummaries
        ));
    }

    #[test]
    fn missing_summaries_dont_resume_when_policy_disabled() {
        let mut s = base();
        s.summaries_present = 0;
        s.generate_summaries = false;
        assert!(matches!(s.next_action(now_ms()), NextAction::Complete));
    }

    #[test]
    fn divergent_commit_runs_incremental() {
        let mut s = base();
        s.head_commit = Some("def".into());
        assert!(matches!(s.next_action(now_ms()), NextAction::Incremental));
    }

    #[test]
    fn missing_indexed_commit_runs_full() {
        let mut s = base();
        s.indexed_commit = None;
        s.entities = 0;
        assert!(matches!(s.next_action(now_ms()), NextAction::Full));
    }

    #[test]
    fn partial_entities_no_indexed_commit_runs_full() {
        let mut s = base();
        s.indexed_commit = None;
        assert!(matches!(s.next_action(now_ms()), NextAction::Full));
    }
}

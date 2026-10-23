//! The record of every identity lifecycle operation (merge, transfer, delete):
//! who ran it, on whom, the names the subject was known by when it started,
//! and its report. It doubles as the per-subject lease that keeps two replicas
//! from operating on one identity at once.

use chrono::{DateTime, Utc};
use eyre::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    Running,
    Complete,
    /// Ended with work left (a failed category or a blocking conflict);
    /// rerunning the operation resumes it.
    Incomplete,
    /// Its replica stopped before finishing and a later operation took over.
    Interrupted,
}

impl OpStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Complete => "complete",
            Self::Incomplete => "incomplete",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "complete" => Self::Complete,
            "incomplete" => Self::Incomplete,
            "interrupted" => Self::Interrupted,
            _ => Self::Running,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LifecycleOp {
    pub id: Uuid,
    pub subject: Uuid,
    /// `merge`, `delete` or `transfer`.
    pub kind: String,
    /// The identity a merge goes into, or a delete's or transfer's recipient.
    pub target: Option<Uuid>,
    /// Who ran it: an identity UUID, or the owner token.
    pub actor: String,
    /// Every name the subject was known by when the operation started.
    pub names: Vec<String>,
    pub status: OpStatus,
    pub report: serde_json::Value,
    pub started_at: DateTime<Utc>,
    pub lease_until: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl LifecycleOp {
    /// The identities the operation acts on: its subject and its target.
    pub fn parties(&self) -> Vec<Uuid> {
        let mut parties: Vec<Uuid> = std::iter::once(self.subject).chain(self.target).collect();
        parties.sort();
        parties.dedup();
        parties
    }
}

pub trait LifecycleOpStore: Send + Sync {
    /// Record `op` as running, unless another operation whose subject or
    /// target is one of `op`'s holds a lease that is still live at `now`
    /// (`false`): a merge into someone must not race their own delete.
    /// Running operations on those identities whose lease has lapsed are
    /// marked [`OpStatus::Interrupted`] in the same step.
    fn claim_op(&self, op: &LifecycleOp, now: DateTime<Utc>) -> Result<bool>;
    /// Extend the lease of `id` while it is still running, recording
    /// `report` as its progress; `false` when it is not, because another
    /// replica took over after it lapsed.
    fn renew_op(
        &self,
        id: Uuid,
        lease_until: DateTime<Utc>,
        report: &serde_json::Value,
    ) -> Result<bool>;
    /// Record how `id` ended, unless it is no longer running (`false`).
    fn finish_op(
        &self,
        id: Uuid,
        status: OpStatus,
        report: &serde_json::Value,
        at: DateTime<Utc>,
    ) -> Result<bool>;
    fn get_op(&self, id: Uuid) -> Result<Option<LifecycleOp>>;
    /// Newest first, optionally for one subject.
    fn list_ops(&self, subject: Option<Uuid>, limit: usize) -> Result<Vec<LifecycleOp>>;
}

#[cfg(test)]
pub(crate) mod contract {
    use super::*;
    use chrono::Duration;

    pub(crate) fn op(subject: Uuid, now: DateTime<Utc>) -> LifecycleOp {
        LifecycleOp {
            id: Uuid::new_v4(),
            subject,
            kind: "merge".into(),
            target: Some(Uuid::new_v4()),
            actor: Uuid::new_v4().to_string(),
            names: vec![subject.to_string(), "slack:U1".into(), "U1".into()],
            status: OpStatus::Running,
            report: serde_json::json!({}),
            started_at: now,
            lease_until: now + Duration::minutes(10),
            finished_at: None,
        }
    }

    /// The behaviour every backend must share.
    pub(crate) fn claims_are_exclusive_per_subject_until_the_lease_lapses(
        store: &dyn LifecycleOpStore,
    ) {
        let now = Utc::now();
        let subject = Uuid::new_v4();
        let first = op(subject, now);
        assert!(store.claim_op(&first, now).unwrap());
        assert!(!store.claim_op(&op(subject, now), now).unwrap());
        assert!(store.claim_op(&op(Uuid::new_v4(), now), now).unwrap());
        let mut into_subject = op(Uuid::new_v4(), now);
        into_subject.target = Some(subject);
        assert!(
            !store.claim_op(&into_subject, now).unwrap(),
            "an operation into an identity under way waits for it"
        );
        let mut from_target = op(first.target.unwrap(), now);
        from_target.target = None;
        assert!(
            !store.claim_op(&from_target, now).unwrap(),
            "the target of an operation under way is held too"
        );

        let later = now + Duration::minutes(11);
        let second = op(subject, later);
        assert!(store.claim_op(&second, later).unwrap());
        let ops = store.list_ops(Some(subject), 10).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].id, second.id);
        assert_eq!(ops[1].status, OpStatus::Interrupted);
        assert_eq!(ops[1].names, first.names);

        let report = serde_json::json!({"sessions": {"count": 3}});
        assert!(store
            .finish_op(second.id, OpStatus::Complete, &report, later)
            .unwrap());
        let done = &store.list_ops(Some(subject), 1).unwrap()[0];
        assert_eq!(done.status, OpStatus::Complete);
        assert_eq!(done.report, report);
        assert_eq!(
            done.finished_at.map(|t| t.timestamp()),
            Some(later.timestamp())
        );
        assert!(store.claim_op(&op(subject, later), later).unwrap());
        assert_eq!(store.list_ops(None, 10).unwrap().len(), 4);
    }

    /// A replica whose lease lapsed and was taken over can neither renew it
    /// nor record an ending over the operation that took over.
    pub(crate) fn a_taken_over_operation_is_fenced(store: &dyn LifecycleOpStore) {
        let now = Utc::now();
        let subject = Uuid::new_v4();
        let first = op(subject, now);
        assert!(store.claim_op(&first, now).unwrap());
        let later = now + Duration::minutes(11);
        assert!(store.claim_op(&op(subject, later), later).unwrap());

        assert!(!store
            .renew_op(
                first.id,
                later + Duration::minutes(30),
                &serde_json::json!({"categories": []})
            )
            .unwrap());
        assert!(!store
            .finish_op(first.id, OpStatus::Complete, &serde_json::json!({}), later)
            .unwrap());
        let ops = store.list_ops(Some(subject), 10).unwrap();
        assert_eq!(ops[1].status, OpStatus::Interrupted);
    }

    /// A renewal extends the lease and records the progress, which a read
    /// by id returns while the operation runs.
    pub(crate) fn renewal_extends_the_lease_and_records_progress(store: &dyn LifecycleOpStore) {
        let now = Utc::now();
        let subject = Uuid::new_v4();
        let first = op(subject, now);
        assert!(store.claim_op(&first, now).unwrap());
        let claimed = store.get_op(first.id).unwrap().expect("recorded");
        assert_eq!(
            (claimed.subject, claimed.names, claimed.status),
            (subject, first.names.clone(), OpStatus::Running)
        );
        let progress = serde_json::json!({"categories": [{"category": "sessions"}]});
        assert!(store
            .renew_op(first.id, now + Duration::minutes(30), &progress)
            .unwrap());
        let later = now + Duration::minutes(20);
        assert!(!store.claim_op(&op(subject, later), later).unwrap());
        let read = store.get_op(first.id).unwrap().expect("recorded");
        assert_eq!((read.status, read.report), (OpStatus::Running, progress));
        assert_eq!(store.get_op(Uuid::new_v4()).unwrap(), None);
    }
}

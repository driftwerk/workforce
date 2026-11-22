//! What a bulk mail job did, batch by batch, so it can carry on where a
//! call's time ran out and be put back with `email.undo`. The hub keeps
//! jobs in Postgres; with none installed, nothing done in bulk is journaled.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::uids::UidSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkFlag {
    Seen,
    Flagged,
}

impl MarkFlag {
    pub fn imap(self) -> &'static str {
        match self {
            MarkFlag::Seen => "\\Seen",
            MarkFlag::Flagged => "\\Flagged",
        }
    }

    /// The search key for messages a change to `on` would change.
    pub fn changes(self, on: bool) -> &'static str {
        match (self, on) {
            (MarkFlag::Seen, true) => "UNSEEN",
            (MarkFlag::Seen, false) => "SEEN",
            (MarkFlag::Flagged, true) => "UNFLAGGED",
            (MarkFlag::Flagged, false) => "FLAGGED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BulkAction {
    /// Out of `from` into `to`. `label` is set for a Gmail archive: the
    /// label leaving the messages, which undo puts back on.
    Move {
        from: String,
        to: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    Mark {
        folder: String,
        flag: MarkFlag,
        on: bool,
    },
}

impl BulkAction {
    pub fn source(&self) -> &str {
        match self {
            BulkAction::Move { from, .. } => from,
            BulkAction::Mark { folder, .. } => folder,
        }
    }
}

/// One batch done: the uids it took in the source folder and, for a move,
/// where they landed (from COPYUID) or, where the server didn't say, their
/// Message-IDs to find them again by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    pub src: UidSet,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst: Option<UidSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst_validity: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub message_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Done,
    Undoing,
    Undone,
    /// Stopped for good; `BulkJob::stopped` says why.
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkJob {
    pub id: String,
    pub owner: String,
    pub account: String,
    pub action: BulkAction,
    /// What was asked for, in words.
    pub asked: String,
    /// The source folder's UIDVALIDITY when the job began.
    pub validity: u32,
    pub pending: UidSet,
    pub done: Vec<Batch>,
    pub state: JobState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
    /// Messages an undo couldn't find to put back.
    #[serde(default)]
    pub unfound: usize,
    pub created: DateTime<Utc>,
}

impl BulkJob {
    pub fn handled(&self) -> usize {
        self.done.iter().map(|b| b.src.len()).sum()
    }
}

#[async_trait]
pub trait MailJournal: Send + Sync {
    /// Keeps `job` as it now stands, replacing what was kept under its id.
    async fn save(&self, job: &BulkJob) -> eyre::Result<()>;
    async fn get(&self, owner: &str, id: &str) -> eyre::Result<Option<BulkJob>>;
    /// The owner's most recent job on `account`.
    async fn latest(&self, owner: &str, account: &str) -> eyre::Result<Option<BulkJob>>;
}

static INSTALLED: OnceLock<Arc<dyn MailJournal>> = OnceLock::new();

/// Sets the process's journal, once.
pub fn install(journal: Arc<dyn MailJournal>) {
    let _ = INSTALLED.set(journal);
}

pub fn installed() -> Option<Arc<dyn MailJournal>> {
    INSTALLED.get().cloned()
}

#[cfg(test)]
pub(crate) mod memory {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    /// A journal kept in memory, for tests.
    #[derive(Default)]
    pub struct MemoryJournal(Mutex<HashMap<String, BulkJob>>);

    #[async_trait]
    impl MailJournal for MemoryJournal {
        async fn save(&self, job: &BulkJob) -> eyre::Result<()> {
            self.0.lock().unwrap().insert(job.id.clone(), job.clone());
            Ok(())
        }

        async fn get(&self, owner: &str, id: &str) -> eyre::Result<Option<BulkJob>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(id)
                .filter(|j| j.owner == owner)
                .cloned())
        }

        async fn latest(&self, owner: &str, account: &str) -> eyre::Result<Option<BulkJob>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .values()
                .filter(|j| j.owner == owner && j.account == account)
                .max_by_key(|j| j.created)
                .cloned())
        }
    }
}

//! What an admin sees and does with an agent's know-how: every entry with
//! its status and counts, and the names others see in it; pin, remove for
//! good, or delete so it may be learned afresh. Never intents, never who,
//! and never a name fewer than `min_users` people used (an addition by hand
//! excepted: reviewing it is the point).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use workforce_common::settings::KnowHowConfig;

use super::{shared_view, Entry, KnowHow, Known};

/// Where an entry stands for everyone but the people who produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Shown to everyone who can reach it, and kept however long unused.
    Pinned,
    /// Enough people produced its names; shown to whoever can reach them.
    Shared,
    /// Added by hand, waiting for an admin to pin it.
    Pending,
    /// Not yet produced by enough people; shown only to those who did.
    Private,
    /// A re-check could not find one of its names; shown to nobody.
    Stale,
}

/// An entry as an admin sees it. `names_hidden` entries carry their kind,
/// status and counts only: their names, and when they were produced, are
/// one person's until enough others produce them.
#[derive(Debug, Clone, Serialize)]
pub struct AdminEntry {
    pub id: i64,
    pub kind: &'static str,
    pub status: Status,
    pub names_hidden: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_id: Option<i64>,
    pub tables: Vec<String>,
    pub columns: Vec<String>,
    pub shape: Option<String>,
    pub wrong: Option<String>,
    pub repo: Option<String>,
    pub path: Option<String>,
    pub symbol: Option<String>,
    pub note: Option<String>,
    pub manual: bool,
    pub people: u32,
    pub uses: u32,
    pub first_seen: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
}

impl AdminEntry {
    /// `e` with `shown`, the names others may see in it, if any.
    fn of(e: &Entry, status: Status, shown: Option<Entry>) -> Self {
        let counts = Self {
            id: e.id,
            kind: e.kind.as_str(),
            status,
            names_hidden: shown.is_none(),
            site: None,
            database_id: None,
            tables: Vec::new(),
            columns: Vec::new(),
            shape: None,
            wrong: None,
            repo: None,
            path: None,
            symbol: None,
            note: None,
            manual: e.manual,
            people: e.people,
            uses: e.uses,
            first_seen: None,
            last_seen: None,
        };
        let Some(s) = shown else {
            return counts;
        };
        let data = !s.site.is_empty();
        Self {
            site: data.then_some(s.site),
            database_id: data.then_some(s.database_id),
            tables: s.tables,
            columns: s.columns,
            shape: s.shape,
            wrong: s.wrong,
            repo: s.repo,
            path: s.path,
            symbol: s.symbol,
            note: s.note,
            first_seen: Some(s.first_seen),
            last_seen: Some(s.last_seen),
            ..counts
        }
    }
}

impl KnowHow {
    /// Every live entry of the agent, most recently used first (those whose
    /// names are hidden last), as an admin sees it.
    pub async fn admin_entries(
        &self,
        agent: &str,
        cfg: &KnowHowConfig,
    ) -> eyre::Result<Vec<AdminEntry>> {
        self.forget_snapshot(agent);
        let known = self.snapshot(agent).await?;
        let min = cfg.min_users.max(1);
        let shared = shared_view(&known, min);
        // A stale entry shows the names it would share were it found again.
        let revived = shared_view(
            &Known {
                entries: Arc::new(
                    known
                        .entries
                        .iter()
                        .map(|e| Entry {
                            stale: false,
                            ..e.clone()
                        })
                        .collect(),
                ),
                ..known.clone()
            },
            min,
        );
        let mut out: Vec<AdminEntry> = known
            .entries
            .iter()
            .map(|e| {
                let (status, shown) = if e.stale {
                    let shown = match e.manual {
                        true => Some(e.clone()),
                        false => revived.get(&e.id).cloned(),
                    };
                    (Status::Stale, shown)
                } else if e.manual {
                    let status = if e.pinned {
                        Status::Pinned
                    } else {
                        Status::Pending
                    };
                    (status, Some(e.clone()))
                } else {
                    match shared.get(&e.id) {
                        Some(s) if e.pinned => (Status::Pinned, Some(s.clone())),
                        Some(s) => (Status::Shared, Some(s.clone())),
                        None => (Status::Private, None),
                    }
                };
                AdminEntry::of(e, status, shown)
            })
            .collect();
        out.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        Ok(out)
    }

    /// Whether an admin may pin the entry: one already shared, or an
    /// addition waiting for review. Never an entry one person produced in
    /// private: pinning would show it to everyone.
    pub async fn pinnable(&self, agent: &str, cfg: &KnowHowConfig, id: i64) -> eyre::Result<bool> {
        self.forget_snapshot(agent);
        let known = self.snapshot(agent).await?;
        let Some(e) = known.entries.iter().find(|e| e.id == id) else {
            return Ok(false);
        };
        Ok(!e.stale && (e.manual || shared_view(&known, cfg.min_users.max(1)).contains_key(&id)))
    }

    /// Pin an entry [`Self::pinnable`] allows.
    pub async fn pin(&self, agent: &str, cfg: &KnowHowConfig, id: i64) -> eyre::Result<bool> {
        if !self.pinnable(agent, cfg, id).await? {
            return Ok(false);
        }
        let done = self.store.set_pinned(agent, id, true).await?;
        self.forget_snapshot(agent);
        Ok(done)
    }

    /// Remove an entry for good: turns never learn it again.
    pub async fn retire(&self, agent: &str, id: i64) -> eyre::Result<bool> {
        let done = self.store.retire(agent, id).await?;
        self.forget_snapshot(agent);
        Ok(done)
    }

    /// Delete an entry and what was recorded for it; turns may learn it
    /// afresh.
    pub async fn delete(&self, agent: &str, id: i64) -> eyre::Result<bool> {
        let done = self.store.delete(agent, id).await?;
        self.forget_snapshot(agent);
        Ok(done)
    }
}

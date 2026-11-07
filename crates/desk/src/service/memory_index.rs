//! Session memory's search index, kept by one worker beside Postgres: the
//! deletions queued by forgets and sweeps first, then closed exchanges not
//! yet in it, a batch at a time within `index_per_minute`, each marked
//! written only once the index has it. A failure backs off and tries again;
//! Postgres is never touched by one. At start and every day it is
//! reconciled with Postgres: what it lacks is written again, and what
//! Postgres no longer has is deleted. A rebuild starts it from nothing.
//!
//! Also here: how much disk session memory takes, measured every few
//! minutes, which holds the harvest when it is full and tells the Floor as
//! it nears that.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::floor::{Answer, Need, NeedKind, Place, Who};
use crate::memory::index::MemoryIndex;
use crate::memory::passage::{
    collection_of, is_ours, passages_of, ref_of_key, ref_purges, Passage, Purge,
};
use crate::memory_index_store::Pending;

/// The most exchanges written in one batch.
const BATCH: i64 = 50;
/// The most deletions run before the outbox is looked at again.
const PURGES: i64 = 20;
/// How often an idle worker looks for work nobody told it of.
const IDLE_EVERY: Duration = Duration::from_secs(15);
/// How often the index is reconciled with Postgres, after the first at
/// start.
const RECONCILE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// The longest a failing index is waited for before it is tried again.
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);
/// Exchanges compared at a time when reconciling.
const RECONCILE_PAGE: i64 = 200;
/// Keys listed at a time when looking for what Postgres no longer has.
const KEYS_PAGE: usize = 1000;
/// How often session memory's disk use is measured.
pub(super) const MEASURE_EVERY: Duration = Duration::from_secs(10 * 60);
/// What one passage takes in the index's store, as estimated: its 768
/// floats (3 KiB), its text (at most 1 600 chars) and its properties and
/// index entries.
pub(super) const PASSAGE_BYTES: u64 = 6 * 1024;
/// How full Desk Memory is when the Floor says so, in percent.
const NEARLY_FULL_AT: u64 = 90;

/// The index worker's share of the desk.
pub(super) struct IndexRun {
    /// The hub's vellev index; none (a hub keeping memories in SQLite),
    /// nothing is indexed and search is by keyword only.
    pub(super) index: Option<Arc<dyn MemoryIndex>>,
    poke: tokio::sync::Notify,
    /// Held while a batch is written or the index reconciled, and while a
    /// rebuild resets it: a batch read before a rebuild is never marked
    /// written after it.
    pub(super) work: tokio::sync::Mutex<()>,
    state: Mutex<IndexState>,
    /// Session memory's disk use in Postgres as last measured, and since
    /// when it has been nearly full.
    pub(super) disk: Mutex<Disk>,
}

#[derive(Debug, Default)]
struct IndexState {
    /// Why the index last failed, while it is failing.
    failing: Option<String>,
    /// When it last caught up with Postgres.
    caught_up: Option<DateTime<Utc>>,
    /// When it was last reconciled.
    reconciled: Option<DateTime<Utc>>,
    /// Deletions set aside, and since when.
    purges_set_aside: (i64, Option<DateTime<Utc>>),
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Disk {
    pub(super) bytes: Option<u64>,
    /// The index's store, as last estimated.
    index_bytes: u64,
    nearly_full_since: Option<DateTime<Utc>>,
}

impl IndexRun {
    pub(super) fn new(index: Option<Arc<dyn MemoryIndex>>) -> Self {
        Self {
            index,
            poke: tokio::sync::Notify::new(),
            work: tokio::sync::Mutex::new(()),
            state: Mutex::default(),
            disk: Mutex::default(),
        }
    }
}

/// How the index stands, for Settings → Memory.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexStatus {
    /// Whether this computer has a search index at all.
    pub installed: bool,
    /// Closed exchanges not yet in it.
    pub backlog: i64,
    /// Deletions not yet run in it.
    pub purges: i64,
    /// Why it isn't answering, while it isn't.
    pub failing: Option<String>,
    pub caught_up: Option<DateTime<Utc>>,
    pub reconciled: Option<DateTime<Utc>>,
    /// Exchanges the index kept refusing, set aside: found by keyword.
    pub set_aside: i64,
    /// Deletions the index kept refusing, set aside until a rebuild.
    pub purges_set_aside: i64,
    /// Session memory's disk use as last measured, in MB: its live data in
    /// Postgres, and the index's estimated from how many passages it holds.
    pub disk_mb: Option<u64>,
    pub max_disk_mb: u64,
}

/// What one turn of the worker found to do.
enum Did {
    /// It did something: go on at once.
    Work,
    /// Nothing waits.
    Idle,
    /// The minute's passages are written: wait until it is over.
    Throttled(Duration),
}

/// How long to wait after `failures` failures in a row: a second, doubling,
/// at most [`BACKOFF_MAX`].
fn backoff(failures: u32) -> Duration {
    let secs = 1u64 << failures.saturating_sub(1).min(16);
    Duration::from_secs(secs).min(BACKOFF_MAX)
}

/// Passages written this minute, against `index_per_minute`.
struct Budget {
    per_minute: u32,
    began: Instant,
    spent: u32,
}

impl Budget {
    fn left(&mut self) -> Result<u32, Duration> {
        if self.began.elapsed() >= Duration::from_secs(60) {
            self.began = Instant::now();
            self.spent = 0;
        }
        match self.per_minute.saturating_sub(self.spent) {
            0 => Err(Duration::from_secs(60).saturating_sub(self.began.elapsed())),
            left => Ok(left),
        }
    }
}

impl Desk {
    /// After memory was forgotten: the index worker has deletions to run,
    /// and the room freed is measured, so a hold on the harvest lifts.
    pub(super) fn memory_forgotten(&self) {
        self.memory_index_poke();
        let desk = self.clone();
        tokio::spawn(async move { desk.measure_memory().await });
    }

    /// Tells the index worker there may be work.
    pub(super) fn memory_index_poke(&self) {
        self.0.index.poke.notify_one();
    }

    /// The index worker, until the desk stops. Without an index it ends at
    /// once.
    pub(super) async fn index_loop(self) {
        let Some(index) = self.0.index.index.clone() else {
            return;
        };
        let mut budget = Budget {
            per_minute: self.0.opts.memory.index_per_minute.max(1),
            began: Instant::now(),
            spent: 0,
        };
        let mut reconcile_at = Instant::now();
        let mut reconcile_failures = 0u32;
        let mut failures = 0u32;
        loop {
            if self.0.cancel.is_cancelled() {
                return;
            }
            if reconcile_at <= Instant::now() {
                // A reconcile that fails is tried again later; the outbox
                // and the deletions go on meanwhile.
                match self.reconcile(&*index).await {
                    Ok(()) => {
                        reconcile_failures = 0;
                        reconcile_at = Instant::now() + RECONCILE_EVERY;
                    }
                    Err(e) => {
                        reconcile_failures += 1;
                        tracing::warn!(
                            error = format!("{e:#}"),
                            "couldn't reconcile Desk Memory's search index; trying again later"
                        );
                        reconcile_at = Instant::now() + backoff(reconcile_failures);
                    }
                }
            }
            let wait = match self.index_step(&*index, &mut budget).await {
                Ok(did) => {
                    if failures > 0 {
                        tracing::info!("Desk Memory's search index is answering again");
                    }
                    failures = 0;
                    self.0.index.state.lock().failing = None;
                    match did {
                        Did::Work => continue,
                        Did::Throttled(wait) => wait,
                        Did::Idle => {
                            self.0.index.state.lock().caught_up = Some(Utc::now());
                            IDLE_EVERY
                        }
                    }
                }
                Err(e) => {
                    failures += 1;
                    let said = format!("{e:#}");
                    if failures == 1 {
                        tracing::warn!(error = %said, "Desk Memory's search index failed; trying again");
                    }
                    self.0.index.state.lock().failing = Some(said);
                    backoff(failures)
                }
            };
            let wait = wait.min(
                reconcile_at
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(10)),
            );
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = self.0.index.poke.notified(), if failures == 0 => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Whether the index answers at all: what tells a request it refused
    /// (to be tried a few more times, then set aside) from an index that
    /// is down (to be waited for).
    async fn answering(index: &dyn MemoryIndex) -> bool {
        index.collections().await.is_ok()
    }

    /// Runs the deletions waiting, or else writes one batch of the outbox.
    /// Both under the work lock, so a rebuild comes wholly before or after.
    async fn index_step(&self, index: &dyn MemoryIndex, budget: &mut Budget) -> eyre::Result<Did> {
        let _work = self.0.index.work.lock().await;
        let purges = self.0.store.purges_pending(PURGES).await?;
        if !purges.is_empty() {
            for p in purges {
                self.run_purge(index, &p.collection, p.id, p.purge.as_ref())
                    .await?;
            }
            return Ok(Did::Work);
        }
        let left = match budget.left() {
            Ok(left) => left,
            Err(wait) => return Ok(Did::Throttled(wait)),
        };
        let pending = self
            .0
            .store
            .index_pending(BATCH.min(i64::from(left)))
            .await?;
        if pending.is_empty() {
            return Ok(Did::Idle);
        }
        let mut by_collection: BTreeMap<String, Vec<&Pending>> = BTreeMap::new();
        for p in &pending {
            by_collection
                .entry(collection_of(p.row.group.as_deref()))
                .or_default()
                .push(p);
        }
        for (collection, rows) in by_collection {
            if index.ensure(&collection).await? {
                // Made afresh: whatever Postgres says it held must be
                // written again.
                let group = rows[0].row.group.as_deref();
                let again = self.0.store.index_requeue_group(group).await?;
                if again > 0 {
                    tracing::info!(
                        collection,
                        again,
                        "a memory collection was made afresh; writing it again"
                    );
                }
            }
            let written = match self.write_rows(index, &collection, &rows, budget).await {
                Ok(()) => rows.clone(),
                Err(e) if !Self::answering(index).await => return Err(e),
                // The index answers, but refused this batch: write each on
                // its own, so one it refuses can't hold up the rest.
                Err(_) => {
                    let mut ok = Vec::new();
                    let mut refused = Vec::new();
                    for row in &rows {
                        match self.write_rows(index, &collection, &[*row], budget).await {
                            Ok(()) => ok.push(*row),
                            Err(e) if !Self::answering(index).await => return Err(e),
                            Err(e) => {
                                tracing::warn!(exchange = %row.row.id, error = format!("{e:#}"), "Desk Memory's search index refused an exchange");
                                refused.push(row.row.id);
                            }
                        }
                    }
                    // A refusal counts against an exchange only when the
                    // index can write: another in this pass was written, or
                    // its own check was. Else the index can't write at all
                    // (its embedder down) and is waited for.
                    if !refused.is_empty() && ok.is_empty() {
                        index.canary().await?;
                    }
                    for id in self.0.store.index_tried(&refused).await? {
                        tracing::warn!(exchange = %id, "an exchange the search index keeps refusing is set aside; it is found by keyword");
                    }
                    ok
                }
            };
            let written: Vec<(Uuid, String)> = written
                .iter()
                .map(|p| (p.row.id, p.version.clone()))
                .collect();
            self.0.store.index_written(&written).await?;
        }
        Ok(Did::Work)
    }

    /// Writes `rows`' passages to `collection`; what it held of an exchange
    /// whose text is now empty goes.
    async fn write_rows(
        &self,
        index: &dyn MemoryIndex,
        collection: &str,
        rows: &[&Pending],
        budget: &mut Budget,
    ) -> eyre::Result<()> {
        let mut passages: Vec<Passage> = Vec::new();
        let mut emptied = Vec::new();
        for p in rows {
            let of = passages_of(&p.row);
            if of.is_empty() {
                emptied.push(p.row.id);
            }
            passages.extend(of);
        }
        if !passages.is_empty() {
            index.write(collection, &passages).await?;
        }
        if !emptied.is_empty() {
            index
                .purge(collection, &Purge::Refs { refs: emptied })
                .await?;
        }
        budget.spent = budget
            .spent
            .saturating_add(u32::try_from(passages.len()).unwrap_or(u32::MAX));
        Ok(())
    }

    async fn run_purge(
        &self,
        index: &dyn MemoryIndex,
        collection: &str,
        id: Uuid,
        purge: Option<&Purge>,
    ) -> eyre::Result<()> {
        let Some(purge) = purge else {
            tracing::warn!(%id, collection, "an index deletion Desk can't read was set aside");
            return self.0.store.purge_done(id).await;
        };
        match index.purge(collection, purge).await {
            Ok(_) => self.0.store.purge_done(id).await,
            Err(e) if !Self::answering(index).await => Err(e),
            // Refused while the index answers otherwise: tried again a few
            // times, then set aside and said on the Floor.
            // It can't write at all: waited for, not counted.
            Err(e) if index.canary().await.is_err() => Err(e),
            Err(e) => {
                tracing::warn!(%id, collection, error = format!("{e:#}"), "Desk Memory's search index refused a deletion");
                if self.0.store.purge_tried(id).await? {
                    tracing::error!(%id, collection, "a deletion the search index keeps refusing is set aside until the index is rebuilt");
                    self.purges_set_aside_read().await;
                }
                Ok(())
            }
        }
    }

    /// Compares the index with Postgres, collection by collection: what it
    /// lacks or holds with another text is queued to be written again, and
    /// what Postgres no longer has (or never had) is queued to be deleted.
    /// A collection of session memory with no group left in Postgres is
    /// dropped. The work lock is held a page at a time, so a rebuild needn't
    /// wait for the whole pass.
    async fn reconcile(&self, index: &dyn MemoryIndex) -> eyre::Result<()> {
        let listed: HashSet<String> = index.collections().await?.into_iter().collect();
        let groups = self.0.store.memory_groups().await?;
        let mut wanted = HashSet::new();
        let (mut again, mut orphans) = (0u64, 0usize);
        for group in &groups {
            let collection = collection_of(group.as_deref());
            wanted.insert(collection.clone());
            if !listed.contains(&collection) {
                let _work = self.0.index.work.lock().await;
                again += self.0.store.index_requeue_group(group.as_deref()).await?;
                continue;
            }
            // Exchanges still to be written before this pass: the index may
            // hold part of each already.
            let unwritten =
                u64::try_from(self.0.store.index_unwritten(group.as_deref()).await?).unwrap_or(0);
            let mut held = 0u64;
            let mut after = None;
            loop {
                let _work = self.0.index.work.lock().await;
                let page = self
                    .0
                    .store
                    .index_held_page(group.as_deref(), after, RECONCILE_PAGE)
                    .await?;
                let Some(last) = page.last() else { break };
                after = Some(last.row.id);
                let pairs: Vec<(String, String)> = page
                    .iter()
                    .flat_map(|p| passages_of(&p.row))
                    .map(|p| (p.key, p.hash))
                    .collect();
                let differing = index.differing(&collection, &pairs).await?;
                held += differing.held;
                let refs: HashSet<Uuid> = differing
                    .keys
                    .iter()
                    .filter_map(|k| ref_of_key(k))
                    .collect();
                if !refs.is_empty() {
                    let refs: Vec<Uuid> = refs.into_iter().collect();
                    again += self.0.store.index_requeue(&refs).await?;
                }
            }
            // It holds more than Postgres says it should, even counting a
            // passage for each exchange still to be written: find what of
            // it Postgres no longer has.
            if index.count(&collection).await? > held + unwritten {
                orphans += self.orphans(index, &collection, group.as_deref()).await?;
            }
        }
        let mut dropped = 0;
        for collection in listed.iter().filter(|c| is_ours(c) && !wanted.contains(*c)) {
            self.0.store.queue_purges(collection, &[Purge::All]).await?;
            dropped += 1;
        }
        // What was set aside is tried again once a day.
        let again_set_aside = self.0.store.index_retry_set_aside().await?;
        if again_set_aside > 0 {
            tracing::info!(
                again_set_aside,
                "trying again the exchanges the search index refused"
            );
        }
        self.0.index.state.lock().reconciled = Some(Utc::now());
        if again > 0 || orphans > 0 || dropped > 0 {
            tracing::info!(
                again,
                orphans,
                dropped,
                "reconciled Desk Memory's search index with Postgres"
            );
        }
        Ok(())
    }

    /// Queues the deletion of what `collection` holds of exchanges Postgres
    /// doesn't have in `group`. How many exchanges' passages.
    async fn orphans(
        &self,
        index: &dyn MemoryIndex,
        collection: &str,
        group: Option<&str>,
    ) -> eyre::Result<usize> {
        let mut gone = Vec::new();
        let mut after = None;
        loop {
            let page = index.keys(collection, after, KEYS_PAGE).await?;
            let refs: Vec<Uuid> = page
                .keys
                .iter()
                .filter_map(|k| ref_of_key(k))
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let kept = self.0.store.index_belonging(group, &refs).await?;
            gone.extend(refs.into_iter().filter(|r| !kept.contains(r)));
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        gone.sort();
        gone.dedup();
        if !gone.is_empty() {
            self.0
                .store
                .queue_purges(collection, &ref_purges(&gone))
                .await?;
        }
        Ok(gone.len())
    }

    /// Starts the search index again from Postgres: each collection of
    /// session memory is dropped, and every exchange written again. Until
    /// it has caught up, what isn't written yet is found by keyword.
    pub async fn rebuild_memory_index(&self) -> Result<u64, DeskError> {
        let Some(index) = self.0.index.index.clone() else {
            return Err(DeskError::Invalid(
                "Desk Memory's search isn't set up on this computer yet, so there's nothing to rebuild.".into(),
            ));
        };
        let _work = self.0.index.work.lock().await;
        let mut collections: HashSet<String> = self
            .0
            .store
            .memory_groups()
            .await?
            .iter()
            .map(|g| collection_of(g.as_deref()))
            .collect();
        match index.collections().await {
            Ok(listed) => collections.extend(listed.into_iter().filter(|c| is_ours(c))),
            Err(e) => tracing::warn!(
                error = format!("{e:#}"),
                "couldn't list Desk Memory's collections; rebuilding those Postgres names"
            ),
        }
        let mut collections: Vec<String> = collections.into_iter().collect();
        collections.sort();
        let queued = self.0.store.rebuild_index(&collections).await?;
        drop(_work);
        self.purges_set_aside_read().await;
        self.memory_index_poke();
        Ok(queued)
    }

    /// How the index stands.
    pub async fn memory_index_status(&self) -> Result<IndexStatus, DeskError> {
        let (failing, caught_up, reconciled) = {
            let s = self.0.index.state.lock();
            (s.failing.clone(), s.caught_up, s.reconciled)
        };
        Ok(IndexStatus {
            installed: self.0.index.index.is_some(),
            backlog: self.0.store.index_backlog(None).await?,
            purges: self.0.store.purges_backlog().await?,
            failing,
            caught_up,
            reconciled,
            set_aside: self.0.store.index_set_aside().await?,
            purges_set_aside: self.0.index.state.lock().purges_set_aside.0,
            disk_mb: self.0.index.disk.lock().bytes.map(|b| b / (1024 * 1024)),
            max_disk_mb: self.0.opts.memory.max_disk_mb,
        })
    }

    /// The index, when there is one and its last try didn't fail: what a
    /// search asks before it falls back to keywords.
    pub(super) fn memory_index_up(&self) -> Option<Arc<dyn MemoryIndex>> {
        let index = self.0.index.index.clone()?;
        self.0.index.state.lock().failing.is_none().then_some(index)
    }

    /// Notes that the index failed a search: until it answers the worker
    /// again, searches are by keyword.
    pub(super) fn memory_index_failed(&self, e: &eyre::Report) {
        tracing::warn!(
            error = format!("{e:#}"),
            "Desk Memory's search index failed a search; searching by keyword"
        );
        self.0.index.state.lock().failing = Some(format!("{e:#}"));
        self.memory_index_poke();
    }

    /// Measures session memory's disk use: its live data in Postgres, and
    /// the index's store estimated from how many passages it holds
    /// ([`PASSAGE_BYTES`] each; the vellev client doesn't say a store's
    /// size, and the node's folder isn't the hub's to read). While the
    /// index doesn't answer, its last estimate stands.
    pub(super) async fn measure_memory(&self) {
        let postgres = match self.0.store.memory_disk_bytes().await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "couldn't measure Desk Memory's disk use"
                );
                return;
            }
        };
        if let Some(index) = self.0.index.index.clone() {
            let mut passages = 0u64;
            let counted: eyre::Result<()> = async {
                for c in index.collections().await? {
                    passages += index.count(&c).await?;
                }
                Ok(())
            }
            .await;
            if counted.is_ok() {
                self.0.index.disk.lock().index_bytes = passages.saturating_mul(PASSAGE_BYTES);
            }
        }
        let index_bytes = self.0.index.disk.lock().index_bytes;
        self.disk_measured(postgres.saturating_add(index_bytes));
        self.purges_set_aside_read().await;
    }

    /// Reads how many deletions are set aside, for the Floor.
    async fn purges_set_aside_read(&self) {
        if let Ok(n) = self.0.store.purges_set_aside().await {
            self.0.index.state.lock().purges_set_aside = n;
        }
    }

    pub(super) fn disk_measured(&self, bytes: u64) {
        let limit = self.0.opts.memory.max_disk_mb.saturating_mul(1024 * 1024);
        let was_full = self.memory_full();
        if was_full && bytes < limit {
            // The hold lifts: every record is read again from where its
            // harvest held.
            let desk = self.clone();
            tokio::spawn(async move {
                if let Ok(all) = desk.0.store.every_session().await {
                    all.iter().for_each(|r| desk.spend_read(r.id));
                }
            });
        }
        let mut d = self.0.index.disk.lock();
        let nearly = bytes.saturating_mul(100) >= limit.saturating_mul(NEARLY_FULL_AT);
        d.bytes = Some(bytes);
        d.nearly_full_since = match (nearly, d.nearly_full_since) {
            (true, Some(since)) => Some(since),
            (true, None) => Some(Utc::now()),
            (false, _) => None,
        };
    }

    /// Whether session memory has used all the disk it may.
    pub(super) fn memory_full(&self) -> bool {
        let limit = self.0.opts.memory.max_disk_mb.saturating_mul(1024 * 1024);
        self.0.index.disk.lock().bytes.is_some_and(|b| b >= limit)
    }

    /// The Floor's need while Desk Memory is nearly or quite full: the
    /// person frees room, or raises the limit. Nothing answers it but that.
    pub fn memory_needs(&self) -> Vec<Need> {
        let mut needs = Vec::new();
        if let (n, Some(since)) = self.0.index.state.lock().purges_set_aside {
            if n > 0 {
                needs.push(desk_memory_need(
                    "memory:purges",
                    crate::memory::PURGES_SET_ASIDE,
                    since,
                ));
            }
        }
        let Some(since) = self.0.index.disk.lock().nearly_full_since else {
            return needs;
        };
        let ask = match self.memory_full() {
            true => crate::memory::FULL,
            false => crate::memory::NEARLY_FULL,
        };
        needs.push(desk_memory_need("memory:disk", ask, since));
        needs
    }
}

/// A need of Desk Memory's own: said, with nothing to click but what the
/// words say to do.
fn desk_memory_need(id: &str, ask: &str, since: DateTime<Utc>) -> Need {
    Need {
        id: id.into(),
        kind: NeedKind::Question,
        who: Who {
            kind: crate::crew::PartyKind::Desk,
            id: "memory".into(),
            name: "Desk Memory".into(),
            engine: None,
        },
        ask: ask.into(),
        why: None,
        options: None,
        chose: None,
        since,
        place: Place::Elsewhere,
        answer: Answer::Reply,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_index_is_waited_for_longer_each_time_to_five_minutes() {
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(5), Duration::from_secs(16));
        assert_eq!(backoff(30), BACKOFF_MAX);
    }

    #[test]
    fn a_minutes_passages_are_written_within_the_limit() {
        let mut b = Budget {
            per_minute: 600,
            began: Instant::now(),
            spent: 0,
        };
        assert_eq!(b.left(), Ok(600));
        b.spent = 600;
        assert!(b.left().is_err_and(|wait| wait <= Duration::from_secs(60)));
        b.began = Instant::now() - Duration::from_secs(61);
        assert_eq!(b.left(), Ok(600), "a new minute");
    }
}

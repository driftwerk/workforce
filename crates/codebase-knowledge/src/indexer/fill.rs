//! Filling a repo's vectors in the vector store from the graph, with no
//! crawl: for a store that came up empty, was lost, or fell behind.
//!
//! A fill goes a page at a time through the repo's entities, then its
//! summaries, in id order, and records in the graph how far it has got
//! after every page, so one that is stopped, by a failure or a restart,
//! goes on from there. Nothing is deleted first: every vector is stored
//! under an id made from what it is of, so storing it again replaces it,
//! and each carries the fill's generation, so that when the fill ends what
//! the store holds of the repo under another generation is what the graph
//! no longer has, and is removed in one step.
//!
//! A fill holds no index lease, so indexing never waits for one. Instead
//! each page is checked after it is written: the graph is read again, and
//! what changed meanwhile is stored again or removed. An index run changes
//! the graph first and the vector store after, so a change the second read
//! missed reaches the store after the page did, and one it saw is dealt
//! with here. Several workers may fill one repo: each page moves the
//! recorded cursor only from where the worker found it, so a worker that
//! lost the race takes up from where the other got to.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use eyre::{eyre, Result};
use tracing::{info, warn};

use crate::graph::entities::EntityId;
use crate::graph::vectors::{FillStage, StoredSummary, VectorFill};
use crate::graph::CodeGraph;
use crate::vector::{CodeVectorStore, VectorCodeEntity, VectorCodeSummary};
use workforce_common::vectorizable::VectorizableText;

/// Entities or summaries per page of a fill.
pub const FILL_PAGE: usize = 200;

/// How many times a page whose entities change while it is stored is
/// stored again before the try is given up as failed.
const SETTLE_TRIES: usize = 5;

/// The most entities read back to check a page: more than a page ever
/// holds, however many were added to its range meanwhile.
const CHECK_LIMIT: usize = 100_000;

/// Files put right per call of [`VectorFiller::settle_pending`].
const PENDING_PAGE: usize = 200;

/// Entities or summaries compared with the store at a time.
pub const VERIFY_PAGE: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifyStage {
    Entities,
    Summaries,
    StrayEntities,
    StraySummaries,
    Done,
}

/// How far a comparison of a repo's graph with the vector store has got
/// ([`VectorFiller::verify_step`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verifying {
    stage: VerifyStage,
    cursor: String,
    /// Entities and summaries compared so far.
    pub compared: usize,
    /// Of those, how many the store lacked or held otherwise.
    pub differing: usize,
    /// What the store held that the graph has not.
    pub strays: usize,
    entities: usize,
    summaries: usize,
    /// Vectors the store holds under what was compared.
    entities_held: usize,
    summaries_held: usize,
}

impl Default for Verifying {
    fn default() -> Self {
        Self {
            stage: VerifyStage::Entities,
            cursor: String::new(),
            compared: 0,
            differing: 0,
            strays: 0,
            entities: 0,
            summaries: 0,
            entities_held: 0,
            summaries_held: 0,
        }
    }
}

impl Verifying {
    fn turn_to(&mut self, stage: VerifyStage) {
        self.stage = stage;
        self.cursor = String::new();
    }

    fn done(&self) -> Verified {
        Verified::Done {
            compared: self.compared,
            differing: self.differing,
            strays: self.strays,
        }
    }
}

/// What a page of a comparison came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verified {
    /// There is more to compare.
    Going,
    /// All compared: so many, so many of them differing, and so many held
    /// that the graph has not. The files of both are noted as pending.
    Done {
        compared: usize,
        differing: usize,
        strays: usize,
    },
    /// So much differed that the repo is being filled again instead.
    Refilling,
}

/// What a vector store should hold of a repo, by the graph, and what it
/// does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Confirmed {
    pub entities_expected: usize,
    pub entities_stored: usize,
    pub summaries_expected: usize,
    pub summaries_stored: usize,
}

impl Confirmed {
    /// Whether the store holds exactly what the graph says it should.
    pub fn agrees(&self) -> bool {
        self.entities_expected == self.entities_stored
            && self.summaries_expected == self.summaries_stored
    }
}

/// Fills a vector store from a graph.
pub struct VectorFiller {
    graph: Arc<CodeGraph>,
    store: Arc<dyn CodeVectorStore>,
    page: usize,
    /// The most a page is read again as, at least.
    check: usize,
    /// How many pages are stored at a time: the one the fill has reached
    /// and those after it.
    ahead: usize,
    /// The pages being stored of each repo, in the order of the graph.
    storing: std::sync::Mutex<HashMap<String, VecDeque<Ahead>>>,
}

/// A page being stored: the entities or summaries of the graph after
/// `from` up to `upto`.
struct Ahead {
    generation: String,
    stage: FillStage,
    from: String,
    upto: String,
    rows: u64,
    storing: tokio::task::JoinHandle<Result<()>>,
}

impl Drop for Ahead {
    /// A page nobody waits for any more is not gone on with.
    fn drop(&mut self) {
        self.storing.abort();
    }
}

/// What stores a page, wherever it runs.
#[derive(Clone)]
struct Pages {
    graph: Arc<CodeGraph>,
    store: Arc<dyn CodeVectorStore>,
    /// The most a page is read again as.
    limit: usize,
}

impl VectorFiller {
    pub fn new(graph: Arc<CodeGraph>, store: Arc<dyn CodeVectorStore>) -> Self {
        Self {
            graph,
            store,
            page: FILL_PAGE,
            check: CHECK_LIMIT,
            ahead: 1,
            storing: std::sync::Mutex::default(),
        }
    }

    /// Fill `page` entities or summaries at a time.
    pub fn paged(mut self, page: usize) -> Self {
        self.page = page.max(1);
        self
    }

    /// Store `ahead` pages at a time: the one the fill has reached and the
    /// ones after it, so that whatever embeds them is not left idle while
    /// one page is written, read again and recorded. Each is recorded only
    /// once it and all before it are stored. One at a time unless told.
    pub fn ahead(mut self, ahead: usize) -> Self {
        self.ahead = ahead.max(1);
        self
    }

    /// Read a page again as no more than `check` entries, or twice the
    /// page if that is more: for showing what happens at the limit without
    /// a hundred thousand entities.
    pub fn checking(mut self, check: usize) -> Self {
        self.check = check.max(1);
        self
    }

    /// Whether the graph records the repo's vectors as complete in the
    /// collection the store has now, looked at afresh when `afresh`.
    pub async fn complete(&self, repo_id: &str, afresh: bool) -> Result<bool> {
        let recorded = self.graph.vectors_complete_in(repo_id).await?;
        Ok(match recorded {
            Some(recorded) => recorded == self.store.identity(afresh).await?,
            None => false,
        })
    }

    /// The fill of `repo_id` under way in the collection the store has
    /// now, or a new one begun: the repo's vectors are recorded incomplete,
    /// and every vector written for it from now carries the new
    /// generation.
    async fn under_way(&self, repo_id: &str) -> Result<VectorFill> {
        let collection = self.store.identity(false).await?;
        let found = self.graph.vector_fill(repo_id).await?;
        let complete_in = self.graph.vectors_complete_in(repo_id).await?;
        match found {
            Some(fill) if fill.stage != FillStage::Done && fill.collection == collection => {
                return Ok(fill)
            }
            // Finished by another worker since this one last looked: there
            // is nothing to begin.
            Some(fill)
                if fill.collection == collection
                    && complete_in.as_deref() == Some(collection.as_str()) =>
            {
                return Ok(fill)
            }
            // Of a collection that is no more: it starts again.
            Some(fill) if fill.stage != FillStage::Done => {
                warn!(
                    repo_id,
                    "the collection a fill of code vectors was in is gone; starting again"
                );
                self.graph.clear_vector_fill(repo_id).await?;
            }
            _ => {}
        }
        self.begin(repo_id, false).await
    }

    /// Begin a fill of `repo_id` afresh, whatever fill there was: every
    /// vector written for the repo from now carries its generation, and
    /// what the store holds of the repo under another goes when it ends.
    /// The repo's vectors are recorded incomplete meanwhile unless
    /// `keep_complete`: kept, the store goes on being searched while it is
    /// filled again, as when a repo already whole in it is only being
    /// gone over, after it was indexed in full again, or at an operator's
    /// word. Returns the fill under way, which is another worker's if one
    /// began first.
    pub async fn begin(&self, repo_id: &str, keep_complete: bool) -> Result<VectorFill> {
        self.graph.clear_vector_fill(repo_id).await?;
        let stats = self.graph.get_stats(repo_id).await?;
        let summaries = self.graph.count_summaries_present(repo_id).await?;
        let begun = VectorFill {
            generation: uuid::Uuid::new_v4().simple().to_string(),
            collection: self.store.identity(true).await?,
            stage: FillStage::Entities,
            cursor: String::new(),
            done: 0,
            total: stats.entity_count as u64 + summaries,
        };
        if !keep_complete {
            self.graph.set_vectors_complete_in(repo_id, None).await?;
        }
        if self
            .graph
            .advance_vector_fill(repo_id, None, &begun)
            .await?
        {
            info!(
                repo_id,
                total = begun.total,
                keep_complete,
                "filling code vectors from the graph"
            );
            return Ok(begun);
        }
        // Another worker began one first.
        self.graph
            .vector_fill(repo_id)
            .await?
            .ok_or_else(|| eyre!("the fill of '{repo_id}' could not be begun"))
    }

    /// Whether a fill of the repo is under way: begun and not finished.
    pub async fn filling(&self, repo_id: &str) -> Result<bool> {
        let fill = self.graph.vector_fill(repo_id).await?;
        Ok(fill.is_some_and(|fill| fill.stage != FillStage::Done))
    }

    /// Compare one page of the repo's graph with the store, from where
    /// `at` says, and move `at` on: entities first, then summaries, each
    /// page by its ids and hashes, which costs the store two counts when it
    /// holds the page as the graph has it (see
    /// [`CodeVectorStore::differing_entities`]). If the store then holds
    /// more of the repo than it holds under what the graph has, and only
    /// then, what it holds is gone through a page at a time for what the
    /// graph has not.
    ///
    /// Nothing is written or removed here. The files of what differs are
    /// noted as pending, and put right as any pending file is, from the
    /// graph as it then is: an index run may be changing the very entities
    /// compared.
    ///
    /// If more than a tenth of what has been compared differs, the store
    /// is not the repo's with a few things wrong but something else, as
    /// one restored to an earlier state may be: the repo's vectors are
    /// recorded incomplete and a fill begun ([`Verified::Refilling`]).
    pub async fn verify_step(&self, repo_id: &str, at: &mut Verifying) -> Result<Verified> {
        let embeddable = |text: &str| VectorizableText::try_new(text).is_ok();
        let mut files: Vec<String> = Vec::new();
        let mut compared = 0;
        match at.stage {
            VerifyStage::Entities => {
                let page = self
                    .graph
                    .entities_page(repo_id, &at.cursor, None, VERIFY_PAGE)
                    .await?;
                match page.last() {
                    Some(last) => {
                        at.cursor = last.id.as_str().to_string();
                        let wanted: Vec<VectorCodeEntity> = page
                            .iter()
                            .map(|entity| VectorCodeEntity::from_code_entity(entity, repo_id, None))
                            .filter(|entity| embeddable(&entity.searchable_content))
                            .collect();
                        at.entities += wanted.len();
                        compared = wanted.len();
                        let differing = self.store.differing_entities(repo_id, &wanted).await?;
                        at.entities_held += differing.held;
                        let places = differing.places.iter();
                        files.extend(places.map(|at| wanted[*at].file_path.clone()));
                    }
                    None => at.turn_to(VerifyStage::Summaries),
                }
            }
            VerifyStage::Summaries => {
                let page = self
                    .graph
                    .summaries_page(repo_id, &at.cursor, None, VERIFY_PAGE)
                    .await?;
                match page.last() {
                    Some(last) => {
                        at.cursor = last.cursor();
                        let wanted: Vec<VectorCodeSummary> = page
                            .iter()
                            .filter(|summary| embeddable(&summary.text))
                            .map(|summary| VectorCodeSummary::of(summary, repo_id, ""))
                            .collect();
                        at.summaries += wanted.len();
                        compared = wanted.len();
                        let differing = self.store.differing_summaries(repo_id, &wanted).await?;
                        at.summaries_held += differing.held;
                        let places = differing.places.iter();
                        files.extend(places.map(|at| wanted[*at].file_path.clone()));
                    }
                    None => {
                        // More held of the repo than is held under what
                        // the graph has: the rest is of entities it has
                        // not, and which they are is looked for. A count
                        // against the graph's would not tell: as many may
                        // be lacking as are held besides.
                        let more = self.store.entity_count(repo_id).await? > at.entities_held
                            || self.store.summary_count(repo_id).await? > at.summaries_held;
                        if !more {
                            return Ok(at.done());
                        }
                        at.turn_to(VerifyStage::StrayEntities);
                    }
                }
            }
            VerifyStage::StrayEntities | VerifyStage::StraySummaries => {
                let summaries = at.stage == VerifyStage::StraySummaries;
                let after = (!at.cursor.is_empty()).then(|| at.cursor.clone());
                let (held, next) = self
                    .store
                    .held_after(repo_id, summaries, after, VERIFY_PAGE)
                    .await?;
                for (key, file_path) in held {
                    if !self.in_graph(&key, summaries).await? {
                        at.strays += 1;
                        files.push(file_path);
                    }
                }
                match (next, summaries) {
                    (Some(next), _) => at.cursor = next,
                    (None, false) => at.turn_to(VerifyStage::StraySummaries),
                    (None, true) => at.stage = VerifyStage::Done,
                }
            }
            VerifyStage::Done => return Ok(at.done()),
        }
        at.compared += compared;
        at.differing += files.len().min(compared);
        files.sort();
        files.dedup();
        if !files.is_empty() {
            self.graph.note_vectors_pending(repo_id, &files).await?;
        }
        if at.compared >= VERIFY_PAGE && at.differing * 10 > at.compared {
            warn!(
                repo_id,
                compared = at.compared,
                differing = at.differing,
                "the vector store holds much of the repo otherwise than the graph has it; filling it again"
            );
            self.begin(repo_id, false).await?;
            return Ok(Verified::Refilling);
        }
        Ok(match at.stage {
            VerifyStage::Done => at.done(),
            _ => Verified::Going,
        })
    }

    /// Whether the graph has what a store holds under `key`: an entity, or
    /// a summary of one ([`VectorCodeSummary::key`]).
    async fn in_graph(&self, key: &str, summary: bool) -> Result<bool> {
        if !summary {
            let entity = EntityId(key.to_string());
            return Ok(self.graph.get_entity(&entity).await?.is_some());
        }
        let Some((entity, granularity)) = key.rsplit_once('|') else {
            return Ok(false);
        };
        let entity = EntityId(entity.to_string());
        Ok(self
            .graph
            .get_summary(&entity, granularity)
            .await?
            .is_some())
    }

    /// Do one page of the repo's fill, beginning one if none is under way,
    /// and return how it then stands: [`FillStage::Done`] once every
    /// entity and summary is stored, what is stale removed, and the
    /// repo's vectors recorded complete. `Err` leaves the fill where it
    /// was, to be gone on with.
    pub async fn step(&self, repo_id: &str) -> Result<VectorFill> {
        let fill = self.under_way(repo_id).await?;
        let mut next = fill.clone();
        match fill.stage {
            FillStage::Entities | FillStage::Summaries => {
                // The pages being stored are taken up, if they are this
                // fill's from where it has got to; otherwise let go.
                let mut storing = {
                    let mut all = self.storing.lock().unwrap_or_else(|e| e.into_inner());
                    all.remove(repo_id).unwrap_or_default()
                };
                let goes_on = storing.front().is_none_or(|first| {
                    first.generation == fill.generation
                        && first.stage == fill.stage
                        && first.from == fill.cursor
                });
                if !goes_on {
                    storing.clear();
                }
                // More are begun, up to as many as are stored at a time.
                let mut from = match storing.back() {
                    Some(last) => last.upto.clone(),
                    None => fill.cursor.clone(),
                };
                while storing.len() < self.ahead {
                    let Some(page) = self.begin_page(repo_id, &fill, &from).await? else {
                        break;
                    };
                    from = page.upto.clone();
                    storing.push_back(page);
                }
                match storing.pop_front() {
                    Some(mut first) => {
                        // Dropped with what is left of them if this fails
                        // or is given up: the fill keeps its place.
                        (&mut first.storing)
                            .await
                            .map_err(|e| eyre!("a page of '{repo_id}' was not stored: {e}"))??;
                        next.cursor = first.upto.clone();
                        next.done += first.rows;
                    }
                    // Nothing after where the fill has got to.
                    None => {
                        next.stage = match fill.stage {
                            FillStage::Entities => FillStage::Summaries,
                            _ => FillStage::Sweep,
                        };
                        next.cursor = String::new();
                    }
                }
                if !self
                    .graph
                    .advance_vector_fill(repo_id, Some(&fill), &next)
                    .await?
                {
                    return self.moved(repo_id).await;
                }
                if !storing.is_empty() {
                    let mut all = self.storing.lock().unwrap_or_else(|e| e.into_inner());
                    all.insert(repo_id.to_string(), storing);
                }
                Ok(next)
            }
            FillStage::Sweep => {
                let swept = self.store.sweep(repo_id, &fill.generation).await?;
                // Looked at afresh: lost and made again meanwhile, the
                // collection holds only what was written since.
                if self.store.identity(true).await? != fill.collection {
                    self.graph.clear_vector_fill(repo_id).await?;
                    return Err(eyre!(
                        "the collection the code vectors of '{repo_id}' were being stored in was made again meanwhile; the fill starts again"
                    ));
                }
                // Recorded complete before the fill is recorded finished, so
                // that a worker that finds it finished finds the vectors
                // complete, and begins no fill of its own.
                self.graph
                    .set_vectors_complete_in(repo_id, Some(&fill.collection))
                    .await?;
                next.stage = FillStage::Done;
                if !self
                    .graph
                    .advance_vector_fill(repo_id, Some(&fill), &next)
                    .await?
                {
                    return self.moved(repo_id).await;
                }
                info!(
                    repo_id,
                    stored = fill.done,
                    swept,
                    "code vectors filled from the graph"
                );
                Ok(next)
            }
            FillStage::Done => Ok(fill),
        }
    }

    /// Give up the pages of the repo being stored ahead of where its fill
    /// has got to, and wait until none of them is running: what they had
    /// yet to send is not sent. A write the store already has may still
    /// land, within the time the store takes over one request. For when
    /// the fill is not to go on for now, an index run having begun: the
    /// fill keeps its recorded place, and goes on from there.
    pub async fn let_go(&self, repo_id: &str) {
        let storing = {
            let mut all = self.storing.lock().unwrap_or_else(|e| e.into_inner());
            all.remove(repo_id).unwrap_or_default()
        };
        // All of them given up before any is waited for: one that ends
        // makes room at the embedder, which a later one not yet given up
        // would take.
        for page in &storing {
            page.storing.abort();
        }
        for mut page in storing {
            let _ = (&mut page.storing).await;
        }
    }

    /// Begin storing the page of the fill's stage after `from`, or `None`
    /// if the graph has nothing after it.
    async fn begin_page(
        &self,
        repo_id: &str,
        fill: &VectorFill,
        from: &str,
    ) -> Result<Option<Ahead>> {
        let pages = Pages {
            graph: Arc::clone(&self.graph),
            store: Arc::clone(&self.store),
            limit: self.check_limit(),
        };
        let (repo, generation) = (repo_id.to_string(), fill.generation.clone());
        let (after, stage) = (from.to_string(), fill.stage);
        let (upto, rows, storing) = match stage {
            FillStage::Entities => {
                let page = self
                    .graph
                    .entities_page(repo_id, from, None, self.page)
                    .await?;
                let Some(last) = page.last() else {
                    return Ok(None);
                };
                let upto = last.id.as_str().to_string();
                let read = pages.as_stored(&repo, &generation, &page);
                let to = upto.clone();
                let storing = tokio::spawn(async move {
                    pages
                        .store_entities(&repo, &generation, &after, &to, read)
                        .await
                });
                (upto, page.len(), storing)
            }
            _ => {
                let page = self
                    .graph
                    .summaries_page(repo_id, from, None, self.page)
                    .await?;
                let Some(last) = page.last() else {
                    return Ok(None);
                };
                let upto = last.cursor();
                let read = Pages::summaries_as_stored(&repo, &generation, &page);
                let to = upto.clone();
                let storing = tokio::spawn(async move {
                    pages
                        .store_summaries(&repo, &generation, &after, &to, read)
                        .await
                });
                (upto, page.len(), storing)
            }
        };
        Ok(Some(Ahead {
            generation: fill.generation.clone(),
            stage,
            from: from.to_string(),
            upto,
            rows: rows as u64,
            storing,
        }))
    }

    /// Where another worker has moved the fill to.
    async fn moved(&self, repo_id: &str) -> Result<VectorFill> {
        self.graph
            .vector_fill(repo_id)
            .await?
            .ok_or_else(|| eyre!("the fill of '{repo_id}' was begun again by an index run"))
    }

    /// The most a page is read again as: more than the page, however long
    /// pages are made, with room for what an index run adds meanwhile.
    fn check_limit(&self) -> usize {
        self.check.max(self.page.saturating_mul(2))
    }

    /// `Err` if a page read again came back as long as it may: there may
    /// be more of it, which would be passed over unstored.
    fn all_read(repo_id: &str, upto: &str, read: usize, limit: usize) -> Result<()> {
        if read >= limit {
            return Err(eyre!(
                "more than {limit} entries of '{repo_id}' came into a page up to '{upto}' while it was stored; the page is tried again"
            ));
        }
        Ok(())
    }

    /// Count what the store should hold of the repo, going through the
    /// graph a page at a time, and what it does hold: every entity and
    /// summary with text to embed, which is what a store embeds.
    pub async fn confirm(&self, repo_id: &str) -> Result<Confirmed> {
        let embeddable = |text: &str| VectorizableText::try_new(text).is_ok();
        let mut confirmed = Confirmed::default();
        let mut after = String::new();
        loop {
            let page = self
                .graph
                .entities_page(repo_id, &after, None, self.page)
                .await?;
            let Some(last) = page.last() else { break };
            after = last.id.as_str().to_string();
            confirmed.entities_expected += page
                .iter()
                .map(|entity| VectorCodeEntity::from_code_entity(entity, repo_id, None))
                .filter(|entity| embeddable(&entity.searchable_content))
                .count();
        }
        let mut after = String::new();
        loop {
            let page = self
                .graph
                .summaries_page(repo_id, &after, None, self.page)
                .await?;
            let Some(last) = page.last() else { break };
            after = last.cursor();
            confirmed.summaries_expected += page
                .iter()
                .filter(|summary| embeddable(&summary.text))
                .count();
        }
        confirmed.entities_stored = self.store.entity_count(repo_id).await?;
        confirmed.summaries_stored = self.store.summary_count(repo_id).await?;
        Ok(confirmed)
    }

    /// The generation vectors written for the repo now carry: its fill's,
    /// or none before any.
    pub async fn generation(graph: &CodeGraph, repo_id: &str) -> Result<String> {
        let fill = graph.vector_fill(repo_id).await?;
        Ok(fill.map(|fill| fill.generation).unwrap_or_default())
    }

    /// Bring the store in line with the graph about one file: drop what it
    /// holds of the file, store the file's entities and summaries as the
    /// graph has them, and go on until the graph read again agrees.
    async fn settle_file(&self, repo_id: &str, file_path: &str) -> Result<()> {
        let held = || async {
            let generation = Self::generation(&self.graph, repo_id).await?;
            let entities = self.graph.find_entities_in_file(repo_id, file_path).await?;
            let entities: Vec<VectorCodeEntity> = entities
                .iter()
                .map(|entity| {
                    VectorCodeEntity::from_code_entity(entity, repo_id, None)
                        .of_generation(&generation)
                })
                .collect();
            let summaries = self.graph.summaries_in_file(repo_id, file_path).await?;
            let summaries: Vec<VectorCodeSummary> = summaries
                .iter()
                .map(|summary| VectorCodeSummary::of(summary, repo_id, &generation))
                .collect();
            eyre::Ok((entities, summaries))
        };
        for _ in 0..SETTLE_TRIES {
            let (entities, summaries) = held().await?;
            self.store
                .delete_entities_by_file(repo_id, file_path)
                .await?;
            self.store
                .delete_summaries_by_file(repo_id, file_path)
                .await?;
            self.store.index_entities(&entities).await?;
            self.store.index_summaries(&summaries).await?;
            if held().await? == (entities, summaries) {
                return Ok(());
            }
        }
        Err(eyre!(
            "'{file_path}' of '{repo_id}' kept changing while its vectors were stored"
        ))
    }

    /// Put the store right about the repo's files noted as pending (see
    /// `vector_pending`), a file at a time, and take each off the list if
    /// it was not noted again meanwhile. Returns how many are still
    /// pending; `Err` at the first that fails, the rest left as they are.
    pub async fn settle_pending(&self, repo_id: &str) -> Result<usize> {
        let pending = self.graph.vectors_pending(repo_id, PENDING_PAGE).await?;
        for (file_path, seq) in &pending {
            self.settle_file(repo_id, file_path).await?;
            self.graph
                .clear_vectors_pending(repo_id, file_path, *seq)
                .await?;
        }
        self.graph.count_vectors_pending(repo_id).await
    }
}

impl Pages {
    /// `page` of the graph's entities, as the store is to hold them in the
    /// fill of `generation`.
    fn as_stored(
        &self,
        repo_id: &str,
        generation: &str,
        page: &[crate::graph::entities::CodeEntity],
    ) -> Vec<VectorCodeEntity> {
        page.iter()
            .map(|entity| {
                VectorCodeEntity::from_code_entity(entity, repo_id, None).of_generation(generation)
            })
            .collect()
    }

    fn summaries_as_stored(
        repo_id: &str,
        generation: &str,
        page: &[StoredSummary],
    ) -> Vec<VectorCodeSummary> {
        page.iter()
            .map(|summary| VectorCodeSummary::of(summary, repo_id, generation))
            .collect()
    }

    /// The entities of the graph after `from` up to `upto`, as the store
    /// is to hold them.
    async fn entities(
        &self,
        repo_id: &str,
        generation: &str,
        from: &str,
        upto: &str,
    ) -> Result<Vec<VectorCodeEntity>> {
        let page = self
            .graph
            .entities_page(repo_id, from, Some(upto), self.limit)
            .await?;
        VectorFiller::all_read(repo_id, upto, page.len(), self.limit)?;
        Ok(self.as_stored(repo_id, generation, &page))
    }

    /// Store the graph's entities after `from` up to `upto`, `read` just
    /// now, and go on until the graph read again holds what was stored.
    async fn store_entities(
        &self,
        repo_id: &str,
        generation: &str,
        from: &str,
        upto: &str,
        read: Vec<VectorCodeEntity>,
    ) -> Result<()> {
        let mut read = Some(read);
        let mut stored: Vec<VectorCodeEntity> = Vec::new();
        for _ in 0..SETTLE_TRIES {
            let held = match read.take() {
                Some(read) => read,
                None => self.entities(repo_id, generation, from, upto).await?,
            };
            let stale: Vec<VectorCodeEntity> = unheld(&stored, &held);
            let fresh: Vec<VectorCodeEntity> = unheld(&held, &stored);
            if stale.is_empty() && fresh.is_empty() {
                return Ok(());
            }
            self.store.remove_entities(&stale).await?;
            self.store.index_entities(&fresh).await?;
            stored = held;
        }
        Err(eyre!(
            "the entities of '{repo_id}' up to '{upto}' kept changing while they were stored"
        ))
    }

    async fn summaries(
        &self,
        repo_id: &str,
        generation: &str,
        from: &str,
        upto: &str,
    ) -> Result<Vec<VectorCodeSummary>> {
        let page: Vec<StoredSummary> = self
            .graph
            .summaries_page(repo_id, from, Some(upto), self.limit)
            .await?;
        VectorFiller::all_read(repo_id, upto, page.len(), self.limit)?;
        Ok(Self::summaries_as_stored(repo_id, generation, &page))
    }

    async fn store_summaries(
        &self,
        repo_id: &str,
        generation: &str,
        from: &str,
        upto: &str,
        read: Vec<VectorCodeSummary>,
    ) -> Result<()> {
        let mut read = Some(read);
        let mut stored: Vec<VectorCodeSummary> = Vec::new();
        for _ in 0..SETTLE_TRIES {
            let held = match read.take() {
                Some(read) => read,
                None => self.summaries(repo_id, generation, from, upto).await?,
            };
            let stale = unheld(&stored, &held);
            let fresh = unheld(&held, &stored);
            if stale.is_empty() && fresh.is_empty() {
                return Ok(());
            }
            self.store.remove_summaries(&stale).await?;
            self.store.index_summaries(&fresh).await?;
            stored = held;
        }
        Err(eyre!(
            "the summaries of '{repo_id}' up to '{upto}' kept changing while they were stored"
        ))
    }
}

/// Those of `these` that `those` does not hold exactly so.
fn unheld<T: Eq + std::hash::Hash + Clone>(these: &[T], those: &[T]) -> Vec<T> {
    let those: std::collections::HashSet<&T> = those.iter().collect();
    these
        .iter()
        .filter(|this| !those.contains(this))
        .cloned()
        .collect()
}

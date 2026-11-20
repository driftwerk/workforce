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
//!
//! A repo the store already holds something of when it is found
//! incomplete is adopted instead: compared with the store a page at a
//! time, only what differs written and what is held besides removed, and
//! searched meanwhile.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;

use eyre::{eyre, Result};
use tracing::{info, warn};

use crate::graph::entities::EntityId;
use crate::graph::vectors::{FillStage, StoredSummary, VectorFill};
use crate::graph::CodeGraph;
use crate::vector::{
    embedding_for, CodeVectorStore, Differing, EmbedReason, HeldRecord, VectorCodeEntity,
    VectorCodeSummary, VERIFY_PAGE,
};
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

/// Whether `text` is anything a store embeds.
fn embeddable(text: &str) -> bool {
    VectorizableText::try_new(text).is_ok()
}

/// Whether the store, holding `held` of a repo's records once `compared`
/// of the graph's were mended, may hold some the graph has not: each
/// mended record is held once, so only then is the count another.
fn strays_held(held: usize, compared: u64) -> bool {
    held as u64 != compared
}

/// A page of the graph compared with the store: what the graph has after
/// `from` up to `upto`, `rows` in all, those with text to embed as the
/// store is to hold them, and how the store differs from them.
struct ComparedPage<T> {
    from: String,
    upto: String,
    rows: u64,
    wanted: Vec<T>,
    differing: Differing,
}

impl<T: Kept> ComparedPage<T> {
    /// The files of the records at `places`.
    fn files<'a>(&'a self, places: &'a [usize]) -> impl Iterator<Item = String> + 'a {
        places.iter().map(|at| self.wanted[*at].file().to_string())
    }
}

/// What a store keeps of the graph, entities or summaries, as an adoption
/// compares and mends it.
trait Kept: Clone + Send + Sync + Sized + 'static {
    /// Whether these are summaries, as the store lists and removes them.
    const SUMMARIES: bool;

    fn file(&self) -> &str;

    /// The text a store embeds.
    fn text(&self) -> &str;

    /// As written in or since the fill or adoption `generation`.
    fn stamped(self, generation: &str) -> Self;

    /// The graph's page after `from` (up to `upto`), at most `limit`, as
    /// the store is to hold it, and where its last row is.
    fn page<'a>(
        graph: &'a CodeGraph,
        repo_id: &'a str,
        from: &'a str,
        upto: Option<&'a str>,
        limit: usize,
    ) -> impl Future<Output = Result<(Vec<Self>, Option<String>)>> + Send + 'a;

    fn differing<'a>(
        store: &'a dyn CodeVectorStore,
        repo_id: &'a str,
        wanted: &'a [Self],
    ) -> impl Future<Output = Result<Differing>> + Send + 'a;

    fn written<'a>(
        store: &'a dyn CodeVectorStore,
        records: &'a [Self],
    ) -> impl Future<Output = Result<usize>> + Send + 'a;
}

impl Kept for VectorCodeEntity {
    const SUMMARIES: bool = false;

    fn file(&self) -> &str {
        &self.file_path
    }

    fn text(&self) -> &str {
        &self.searchable_content
    }

    fn stamped(self, generation: &str) -> Self {
        self.of_generation(generation)
    }

    async fn page(
        graph: &CodeGraph,
        repo_id: &str,
        from: &str,
        upto: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<Self>, Option<String>)> {
        let page = graph.entities_page(repo_id, from, upto, limit).await?;
        let last = page.last().map(|entity| entity.id.as_str().to_string());
        let held = page
            .iter()
            .map(|entity| VectorCodeEntity::from_code_entity(entity, repo_id, None))
            .collect();
        Ok((held, last))
    }

    fn differing<'a>(
        store: &'a dyn CodeVectorStore,
        repo_id: &'a str,
        wanted: &'a [Self],
    ) -> impl Future<Output = Result<Differing>> + Send + 'a {
        store.differing_entities(repo_id, wanted)
    }

    fn written<'a>(
        store: &'a dyn CodeVectorStore,
        records: &'a [Self],
    ) -> impl Future<Output = Result<usize>> + Send + 'a {
        store.index_entities(records)
    }
}

impl Kept for VectorCodeSummary {
    const SUMMARIES: bool = true;

    fn file(&self) -> &str {
        &self.file_path
    }

    fn text(&self) -> &str {
        &self.summary_text
    }

    fn stamped(self, generation: &str) -> Self {
        Self {
            generation: generation.to_string(),
            ..self
        }
    }

    async fn page(
        graph: &CodeGraph,
        repo_id: &str,
        from: &str,
        upto: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<Self>, Option<String>)> {
        let page = graph.summaries_page(repo_id, from, upto, limit).await?;
        let last = page.last().map(StoredSummary::cursor);
        let held = page
            .iter()
            .map(|summary| VectorCodeSummary::of(summary, repo_id, ""))
            .collect();
        Ok((held, last))
    }

    fn differing<'a>(
        store: &'a dyn CodeVectorStore,
        repo_id: &'a str,
        wanted: &'a [Self],
    ) -> impl Future<Output = Result<Differing>> + Send + 'a {
        store.differing_summaries(repo_id, wanted)
    }

    fn written<'a>(
        store: &'a dyn CodeVectorStore,
        records: &'a [Self],
    ) -> impl Future<Output = Result<usize>> + Send + 'a {
        store.index_summaries(records)
    }
}

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
    /// collection the store has now, looked at afresh when `afresh`: so it
    /// is, whatever is recorded, before a fill or adoption goes on.
    pub async fn complete(&self, repo_id: &str, afresh: bool) -> Result<bool> {
        let held = self.store.identity(afresh).await?;
        let recorded = self.graph.vectors_complete_in(repo_id).await?;
        Ok(recorded.is_some_and(|recorded| recorded == held))
    }

    /// The fill or adoption of `repo_id` under way in the collection the
    /// store has now, or a new one begun ([`Self::begun`]).
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
        self.begun(repo_id).await
    }

    /// Begin what brings the store in line with the graph about a repo it
    /// is not recorded complete in. The tenant's vectors are first made
    /// sure of as this graph's, and both collections looked at afresh, so
    /// that nothing is read or written of a collection that is another's,
    /// or of another shape. Holding nothing of the repo, the store is
    /// filled ([`Self::begin`]); holding anything, the repo is adopted: its
    /// vectors are searched meanwhile, and only what differs is written.
    async fn begun(&self, repo_id: &str) -> Result<VectorFill> {
        let collection = self.store.identity(true).await?;
        let held =
            self.store.entity_count(repo_id).await? + self.store.summary_count(repo_id).await?;
        if held == 0 {
            return self.begin_in(repo_id, false, collection).await;
        }
        // Whatever row there is, such as one of a stage this build cannot
        // read, is not gone on with.
        self.graph.clear_vector_fill(repo_id).await?;
        let adopting = VectorFill {
            generation: uuid::Uuid::new_v4().simple().to_string(),
            collection,
            stage: FillStage::AdoptEntities,
            cursor: String::new(),
            done: 0,
            total: self.total(repo_id).await?,
        };
        if !self
            .graph
            .advance_vector_fill(repo_id, None, &adopting)
            .await?
        {
            return self.begun_elsewhere(repo_id).await;
        }
        info!(
            repo_id,
            collection = %adopting.collection,
            held,
            "adopting the code vectors the store already holds of the repo"
        );
        Ok(adopting)
    }

    /// Entities and summaries the graph has of the repo.
    async fn total(&self, repo_id: &str) -> Result<u64> {
        let stats = self.graph.get_stats(repo_id).await?;
        let summaries = self.graph.count_summaries_present(repo_id).await?;
        Ok(stats.entity_count as u64 + summaries)
    }

    /// The fill another worker began first.
    async fn begun_elsewhere(&self, repo_id: &str) -> Result<VectorFill> {
        self.graph
            .vector_fill(repo_id)
            .await?
            .ok_or_else(|| eyre!("the fill of '{repo_id}' could not be begun"))
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
        let collection = self.store.identity(true).await?;
        self.begin_in(repo_id, keep_complete, collection).await
    }

    /// [`Self::begin`] in `collection`, the store's looked at afresh.
    async fn begin_in(
        &self,
        repo_id: &str,
        keep_complete: bool,
        collection: String,
    ) -> Result<VectorFill> {
        self.graph.clear_vector_fill(repo_id).await?;
        let begun = VectorFill {
            generation: uuid::Uuid::new_v4().simple().to_string(),
            collection,
            stage: FillStage::Entities,
            cursor: String::new(),
            done: 0,
            total: self.total(repo_id).await?,
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
        self.begun_elsewhere(repo_id).await
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
        let mut files: Vec<String> = Vec::new();
        let mut compared = 0;
        match at.stage {
            VerifyStage::Entities => match self
                .compared::<VectorCodeEntity>(repo_id, &at.cursor, None)
                .await?
            {
                Some(page) => {
                    at.cursor = page.upto.clone();
                    at.entities += page.wanted.len();
                    compared = page.wanted.len();
                    at.entities_held += page.differing.held;
                    files.extend(page.files(&page.differing.places));
                }
                None => at.turn_to(VerifyStage::Summaries),
            },
            VerifyStage::Summaries => {
                match self
                    .compared::<VectorCodeSummary>(repo_id, &at.cursor, None)
                    .await?
                {
                    Some(page) => {
                        at.cursor = page.upto.clone();
                        at.summaries += page.wanted.len();
                        compared = page.wanted.len();
                        at.summaries_held += page.differing.held;
                        files.extend(page.files(&page.differing.places));
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
                let (strays, next) = self.strays(repo_id, summaries, after).await?;
                at.strays += strays.len();
                files.extend(strays.into_iter().map(|stray| stray.file_path));
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
        // The verify's alone: an adoption mends whatever share differs.
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

    /// How many a page of the graph after `from` is read as: a page, or,
    /// read again up to where it ended, as many as may have come into it.
    fn read_limit(&self, upto: Option<&str>) -> usize {
        upto.map_or(VERIFY_PAGE, |_| self.check_limit())
    }

    /// The graph's entities or summaries after `from` (up to `upto`, if
    /// read again) compared with the store; `None` if the graph has none
    /// there.
    async fn compared<T: Kept>(
        &self,
        repo_id: &str,
        from: &str,
        upto: Option<&str>,
    ) -> Result<Option<ComparedPage<T>>> {
        let limit = self.read_limit(upto);
        let (page, last) = T::page(&self.graph, repo_id, from, upto, limit).await?;
        if let Some(upto) = upto {
            Self::all_read(repo_id, upto, page.len(), limit)?;
        }
        let Some(last) = last else {
            return Ok(None);
        };
        let rows = page.len() as u64;
        let wanted: Vec<T> = page
            .into_iter()
            .filter(|record| embeddable(record.text()))
            .collect();
        let differing = match wanted.is_empty() {
            true => Differing::default(),
            false => T::differing(self.store.as_ref(), repo_id, &wanted).await?,
        };
        Ok(Some(ComparedPage {
            from: from.to_string(),
            upto: upto.map_or(last, str::to_string),
            rows,
            wanted,
            differing,
        }))
    }

    /// A page of what the store holds of the repo's entities, or
    /// `summaries`, after `after`: those of it the graph has not, and where
    /// the next page starts.
    async fn strays(
        &self,
        repo_id: &str,
        summaries: bool,
        after: Option<String>,
    ) -> Result<(Vec<HeldRecord>, Option<String>)> {
        let (held, next) = self
            .store
            .held_after(repo_id, summaries, after, VERIFY_PAGE)
            .await?;
        let mut strays = Vec::new();
        for record in held {
            if !self.in_graph(&record.key, summaries).await? {
                strays.push(record);
            }
        }
        Ok((strays, next))
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
                next.stage = FillStage::Done;
                let finished = self.finish(repo_id, &fill, next).await?;
                info!(
                    repo_id,
                    stored = fill.done,
                    swept,
                    "code vectors filled from the graph"
                );
                Ok(finished)
            }
            FillStage::Done => Ok(fill),
            FillStage::AdoptEntities
            | FillStage::AdoptStrayEntities
            | FillStage::AdoptSummaries
            | FillStage::AdoptStraySummaries => {
                embedding_for(EmbedReason::Adopt, self.adopted(repo_id, &fill)).await
            }
        }
    }

    /// Record the repo's vectors complete in the collection of `fill`, and
    /// `fill` moved on to `done`, if the store has that collection still,
    /// looked at afresh: lost and made again meanwhile, the collection holds
    /// only what was written since, and what `fill` did is begun again.
    async fn finish(
        &self,
        repo_id: &str,
        fill: &VectorFill,
        done: VectorFill,
    ) -> Result<VectorFill> {
        if self.store.identity(true).await? != fill.collection {
            self.graph.clear_vector_fill(repo_id).await?;
            return Err(eyre!(
                "the collection of the code vectors of '{repo_id}' was made again meanwhile, so they are begun again"
            ));
        }
        // Recorded complete before the fill is recorded finished, so
        // that a worker that finds it finished finds the vectors
        // complete, and begins no fill of its own.
        self.graph
            .set_vectors_complete_in(repo_id, Some(&fill.collection))
            .await?;
        if !self
            .graph
            .advance_vector_fill(repo_id, Some(fill), &done)
            .await?
        {
            return self.moved(repo_id).await;
        }
        Ok(done)
    }

    /// One page of an adoption, or the turn to its next stage, or its
    /// finish once there is nothing left to compare: how it then stands.
    async fn adopted(&self, repo_id: &str, fill: &VectorFill) -> Result<VectorFill> {
        let next = match fill.stage {
            FillStage::AdoptEntities => self.adopt_page::<VectorCodeEntity>(repo_id, fill).await?,
            FillStage::AdoptSummaries => {
                self.adopt_page::<VectorCodeSummary>(repo_id, fill).await?
            }
            stage => {
                let summaries = stage == FillStage::AdoptStraySummaries;
                self.adopt_strays(repo_id, fill, summaries).await?
            }
        };
        if next.stage == FillStage::Done {
            let finished = self.finish(repo_id, fill, next).await?;
            info!(
                repo_id,
                compared = fill.done,
                "code vectors adopted from what the store held of the repo"
            );
            return Ok(finished);
        }
        if !self
            .graph
            .advance_vector_fill(repo_id, Some(fill), &next)
            .await?
        {
            return self.moved(repo_id).await;
        }
        Ok(next)
    }

    /// Compare the graph's page after where the adoption has got with the
    /// store and mend it; at the graph's end, turn to what the store may
    /// hold besides, if its count says it may.
    async fn adopt_page<T: Kept>(&self, repo_id: &str, fill: &VectorFill) -> Result<VectorFill> {
        let Some(page) = self.compared::<T>(repo_id, &fill.cursor, None).await? else {
            let mut held = self.store.entity_count(repo_id).await?;
            if T::SUMMARIES {
                held += self.store.summary_count(repo_id).await?;
            }
            return Ok(VectorFill {
                stage: after_adopting(fill.stage, strays_held(held, fill.done)),
                cursor: String::new(),
                ..fill.clone()
            });
        };
        let (upto, rows) = (page.upto.clone(), page.rows);
        self.mend(repo_id, &fill.generation, page).await?;
        Ok(VectorFill {
            cursor: upto,
            done: fill.done + rows,
            ..fill.clone()
        })
    }

    /// Bring the store in line with a compared page: what it lacks is
    /// written, stamped with `generation`, and only then what it holds
    /// besides removed, so that nothing of the page goes unsearchable;
    /// then the page is read and compared again, until it agrees.
    async fn mend<T: Kept>(
        &self,
        repo_id: &str,
        generation: &str,
        page: ComparedPage<T>,
    ) -> Result<()> {
        let (from, upto) = (page.from.clone(), page.upto.clone());
        let mut first = Some(page);
        for _ in 0..SETTLE_TRIES {
            let compared = match first.take() {
                Some(page) => Some(page),
                None => self.compared::<T>(repo_id, &from, Some(&upto)).await?,
            };
            let Some(compared) = compared.filter(|page| !page.differing.places.is_empty()) else {
                return Ok(());
            };
            let missing: Vec<T> = compared
                .differing
                .missing
                .iter()
                .map(|at| compared.wanted[*at].clone().stamped(generation))
                .collect();
            if !missing.is_empty() {
                T::written(self.store.as_ref(), &missing).await?;
            }
            if !compared.differing.stale.is_empty() {
                self.store
                    .remove_held(T::SUMMARIES, &compared.differing.stale)
                    .await?;
            }
        }
        Err(eyre!(
            "the code vectors of '{repo_id}' up to '{upto}' kept changing while they were adopted"
        ))
    }

    /// Remove a page of what the store holds of the repo that the graph
    /// has not, and move the adoption on past it.
    async fn adopt_strays(
        &self,
        repo_id: &str,
        fill: &VectorFill,
        summaries: bool,
    ) -> Result<VectorFill> {
        let after = (!fill.cursor.is_empty()).then(|| fill.cursor.clone());
        let (strays, next) = self.strays(repo_id, summaries, after).await?;
        self.removed_strays(repo_id, summaries, &strays).await?;
        Ok(match next {
            Some(next) => VectorFill {
                cursor: next,
                ..fill.clone()
            },
            None => VectorFill {
                stage: after_adopting(fill.stage, false),
                cursor: String::new(),
                ..fill.clone()
            },
        })
    }

    /// Remove `strays`, then look each up in the graph again: an index run
    /// may have added one since, and its own write been what was removed,
    /// so its file is noted as pending, to be written back.
    async fn removed_strays(
        &self,
        repo_id: &str,
        summaries: bool,
        strays: &[HeldRecord],
    ) -> Result<()> {
        if strays.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = strays.iter().map(|stray| stray.id.clone()).collect();
        self.store.remove_held(summaries, &ids).await?;
        let mut back: Vec<String> = Vec::new();
        for stray in strays {
            if self.in_graph(&stray.key, summaries).await? {
                back.push(stray.file_path.clone());
            }
        }
        back.sort();
        back.dedup();
        if !back.is_empty() {
            self.graph.note_vectors_pending(repo_id, &back).await?;
        }
        Ok(())
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
                let storing = tokio::spawn(embedding_for(EmbedReason::Refill, async move {
                    pages
                        .store_entities(&repo, &generation, &after, &to, read)
                        .await
                }));
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
                let storing = tokio::spawn(embedding_for(EmbedReason::Refill, async move {
                    pages
                        .store_summaries(&repo, &generation, &after, &to, read)
                        .await
                }));
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
    /// graph has them, and go on until the graph read again agrees. A file
    /// a store that keeps track already holds exactly as the graph has it
    /// is left as it is, unless a fill is under way that sweeps what it
    /// did not write.
    async fn settle_file(&self, repo_id: &str, file_path: &str) -> Result<()> {
        let held = || async {
            let fill = self.graph.vector_fill(repo_id).await?;
            let sweeping = fill.as_ref().is_some_and(|fill| fill.stage.sweeps());
            let generation = fill.map(|fill| fill.generation).unwrap_or_default();
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
            eyre::Ok((sweeping, entities, summaries))
        };
        embedding_for(EmbedReason::Pending, async {
            for _ in 0..SETTLE_TRIES {
                let (sweeping, entities, summaries) = held().await?;
                let skipped = self.store.tracks_completeness()
                    && !sweeping
                    && self
                        .held_exactly(repo_id, file_path, &entities, &summaries)
                        .await?;
                if !skipped {
                    self.store
                        .delete_entities_by_file(repo_id, file_path)
                        .await?;
                    self.store
                        .delete_summaries_by_file(repo_id, file_path)
                        .await?;
                    self.store.index_entities(&entities).await?;
                    self.store.index_summaries(&summaries).await?;
                }
                if held().await? == (sweeping, entities, summaries) {
                    return Ok(());
                }
            }
            Err(eyre!(
                "'{file_path}' of '{repo_id}' kept changing while its vectors were stored"
            ))
        })
        .await
    }

    /// Whether the store holds one file's `entities` and `summaries`, as
    /// the graph has them, and nothing else of it: each with text to embed
    /// held once as it is, and as many held of the file as those.
    async fn held_exactly(
        &self,
        repo_id: &str,
        file_path: &str,
        entities: &[VectorCodeEntity],
        summaries: &[VectorCodeSummary],
    ) -> Result<bool> {
        let entities: Vec<VectorCodeEntity> = entities
            .iter()
            .filter(|entity| embeddable(&entity.searchable_content))
            .cloned()
            .collect();
        let summaries: Vec<VectorCodeSummary> = summaries
            .iter()
            .filter(|summary| embeddable(&summary.summary_text))
            .cloned()
            .collect();
        if self.store.file_held(repo_id, file_path).await? != entities.len() + summaries.len() {
            return Ok(false);
        }
        let entities_differ = !entities.is_empty()
            && !self
                .store
                .differing_entities(repo_id, &entities)
                .await?
                .places
                .is_empty();
        if entities_differ {
            return Ok(false);
        }
        Ok(summaries.is_empty()
            || self
                .store
                .differing_summaries(repo_id, &summaries)
                .await?
                .places
                .is_empty())
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

/// The stage of an adoption after `stage`'s last page, where the store may
/// hold `strays` of the repo besides what was compared.
fn after_adopting(stage: FillStage, strays: bool) -> FillStage {
    match (stage, strays) {
        (FillStage::AdoptEntities, true) => FillStage::AdoptStrayEntities,
        (FillStage::AdoptEntities, false) | (FillStage::AdoptStrayEntities, _) => {
            FillStage::AdoptSummaries
        }
        (FillStage::AdoptSummaries, true) => FillStage::AdoptStraySummaries,
        _ => FillStage::Done,
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

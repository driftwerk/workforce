//! Shadow mode's known-item runs of artifacts (DW-676, PR 10; design r2
//! section 6.5, ladder steps 1 and 2).
//!
//! The two sides embed with different models (Weaviate's t2v module, the
//! node's bge), so their agreement measures how they differ, not which is
//! better. Each night the leader draws artifacts from Postgres, by the
//! same seeded rule as memories ([`crate::known_items`]), makes a query of
//! each from its title and content, and asks the serving Weaviate index
//! and the vellev mirror, one attempt each (A7), where the artifact comes:
//! recall@5, recall@10 and MRR, from the same rows memories' runs make.
//! With the opt-in judge on, an LLM also grades what both found, for each
//! side's nDCG@5 ([`crate::known_item_judge`]).
//!
//! A run is made only while the mirror is in step and the serving index
//! complete, as the read shadow compares only then; it holds one of the
//! artifact shadow's permits an item, and makes no run while it is paused.
//! It records no title, content, owner or query: an artifact by its id.
//!
//! An artifact written within [`SETTLE`], or still noted for either index
//! to take, is not drawn: one side may not hold it as it is yet.
//!
//! **Limitation: these queries are easy**, as memories' are (see
//! [`crate::known_items`]): each is an excerpt of the artifact it is to
//! find. Artifacts are asked in the `window` mode only.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use eyre::Result;
use workforce_common::settings::KnownItemSettings;
use workforce_common::shadow::{RankMetrics, Shadow};
use workforce_common::shadow_postgres::{
    ArtifactText, KnownItemClaim, KnownItemOutcome, KnownItemRow, KnownItemSide, KnownItemStatus,
    KnownItemsDrawn, Population, QueryMode, ShadowStore,
};
use workforce_orchestrator::artifact_store::ArtifactIndexer;

use crate::known_item_judge::Judge;
use crate::known_items::{
    ask_each, draw_key, query_of, run_seed, Asking, Excluded, Nightly, Ran, Recorder, Smallest,
    DERIVATION,
};

/// The store a run is recorded under.
pub const STORE: &str = "artifacts";
/// How deep each side is asked.
pub const DEPTH: usize = 10;
/// Artifacts a page of the draw reads.
pub const DRAW_PAGE: usize = 100;
/// Artifacts written within this long are not drawn: the mirror may not
/// hold them yet.
pub const SETTLE: Duration = Duration::from_secs(30 * 60);

/// Where artifacts are read from: Postgres, through the shadow tables'
/// pool, in a hub.
#[async_trait]
pub trait Artifacts: Send + Sync {
    async fn page(&self, after: Option<&str>, limit: usize) -> Result<Vec<ArtifactText>>;
    async fn by_id(&self, ids: &[String], chars: usize) -> Result<Vec<ArtifactText>>;
    /// Items of artifacts the judge graded in the runs for `day`.
    async fn judged_on(&self, day: NaiveDate) -> Result<i64>;
}

#[async_trait]
impl Artifacts for ShadowStore {
    async fn page(&self, after: Option<&str>, limit: usize) -> Result<Vec<ArtifactText>> {
        self.artifact_page(after, limit).await
    }
    async fn by_id(&self, ids: &[String], chars: usize) -> Result<Vec<ArtifactText>> {
        self.artifacts_by_id(ids, chars).await
    }
    async fn judged_on(&self, day: NaiveDate) -> Result<i64> {
        ShadowStore::judged_on(self, STORE, day).await
    }
}

/// One artifact drawn, and the query made of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactItem {
    pub id: String,
    pub query: String,
}

/// The query made of an artifact: a window of its title and content's
/// words ([`query_of`]).
pub fn artifact_query(seed: u64, artifact: &ArtifactText) -> Option<String> {
    query_of(
        seed,
        &artifact.id,
        &format!("{} {}", artifact.title, artifact.content),
    )
}

/// A draw of artifacts under way.
pub struct ArtifactDraw {
    seed: u64,
    settled_before: DateTime<Utc>,
    kept: Smallest<String>,
    drawn: KnownItemsDrawn,
}

impl ArtifactDraw {
    pub fn new(seed: u64, items: usize, settled_before: DateTime<Utc>) -> Self {
        Self {
            seed,
            settled_before,
            kept: Smallest::new(items),
            drawn: KnownItemsDrawn::default(),
        }
    }

    pub fn offer(&mut self, artifact: &ArtifactText) {
        self.drawn.population += 1;
        match self.query(artifact) {
            Ok(query) => {
                let entry = (draw_key(self.seed, &artifact.id), artifact.id.clone());
                // An id is read once: Postgres's primary key.
                self.kept.offer(entry, query, |_| false);
            }
            Err(why) => *self.drawn.excluded.entry(why.as_str()).or_default() += 1,
        }
    }

    fn query(&self, artifact: &ArtifactText) -> Result<String, Excluded> {
        if artifact.updated_at >= self.settled_before {
            return Err(Excluded::TooRecent);
        }
        if artifact.pending {
            return Err(Excluded::Pending);
        }
        let query = artifact_query(self.seed, artifact).ok_or(Excluded::TooShort)?;
        if workforce_common::vectorizable::VectorizableText::try_new(query.as_str()).is_err() {
            return Err(Excluded::NotVectorizable);
        }
        Ok(query)
    }

    /// The items drawn, in the order of their keys, and what was drawn.
    pub fn finish(mut self) -> (Vec<ArtifactItem>, KnownItemsDrawn) {
        self.drawn.drawn = self.kept.len();
        let items = self
            .kept
            .into_values()
            .map(|((_, id), query)| ArtifactItem { id, query })
            .collect();
        (items, self.drawn)
    }
}

/// The judge as a run is to use it: made afresh for each run, so that one
/// turned on, or an LLM set up since, is used from the next run on.
pub type JudgeSource = Arc<dyn Fn() -> Result<Option<Judge>> + Send + Sync>;

/// A night's run of artifacts, ready to be made.
pub struct ArtifactRunner {
    pub source: Arc<dyn Artifacts>,
    /// Weaviate's index, which serves.
    pub serving: Arc<dyn ArtifactIndexer>,
    /// vellev's mirror, as the read shadow asks it: one attempt.
    pub mirror: Arc<dyn ArtifactIndexer>,
    /// The artifact shadow's kernel, once the hub has one.
    pub kernel: Arc<dyn Fn() -> Option<Arc<Shadow>> + Send + Sync>,
    /// Whether the mirror is in step, as the read shadow judges it.
    pub in_step: Arc<dyn Fn() -> bool + Send + Sync>,
    pub recorder: Arc<dyn Recorder>,
    pub judge: JudgeSource,
    pub settings: KnownItemSettings,
    /// Rank-biased overlap's p, as the artifact shadow compares with.
    pub rbo_p: f64,
    pub settle: Duration,
    pub permit_wait: Duration,
    pub page_pace: Duration,
}

#[async_trait]
impl Nightly for ArtifactRunner {
    fn store(&self) -> &'static str {
        STORE
    }
    fn window_utc(&self) -> (u32, u32) {
        (self.settings.hour_utc, self.settings.until_hour_utc)
    }
    async fn run(
        &self,
        day: NaiveDate,
        now: DateTime<Utc>,
        go: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<Option<Ran>> {
        if self.settings.items == 0 {
            return Ok(None);
        }
        let Some(kernel) = (self.kernel)() else {
            return Ok(None);
        };
        if !kernel.enabled() || !(self.in_step)() || !self.serving.complete().await? {
            return Ok(None);
        }
        let seed = run_seed(self.settings.seed, day);
        let claim = KnownItemClaim {
            store: STORE.to_string(),
            run_day: day,
            base_seed: self.settings.seed,
            run_seed: seed,
            derivation: DERIVATION,
            k: DEPTH,
            alpha_serving: 1.0,
            items_wanted: self.settings.items,
        };
        let Some(run) = self.recorder.claim(&claim).await? else {
            return Ok(None);
        };
        let made = self.made(run, seed, day, now, &kernel, go).await;
        let (outcome, error, drawn, items) = match made {
            Ok(made) => made,
            Err(e) => (
                KnownItemOutcome::Failed,
                Some(format!("{e:#}")),
                KnownItemsDrawn::default(),
                0,
            ),
        };
        self.recorder.finish(run, outcome, error.as_deref()).await?;
        Ok(Some(Ran {
            run,
            outcome,
            error,
            drawn,
            items,
        }))
    }
}

/// The judge for one run, and how many more it may judge.
struct Judging {
    judge: Judge,
    left: AtomicUsize,
}

impl ArtifactRunner {
    async fn made(
        &self,
        run: i64,
        seed: u64,
        day: NaiveDate,
        now: DateTime<Utc>,
        kernel: &Shadow,
        go: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<(KnownItemOutcome, Option<String>, KnownItemsDrawn, usize)> {
        let Some((items, drawn)) = self.draw(seed, now, go).await? else {
            return Ok((
                KnownItemOutcome::Incomplete,
                Some(crate::known_items::STOPPED_IN_DRAW.to_string()),
                KnownItemsDrawn::default(),
                0,
            ));
        };
        self.recorder.drawn(run, &drawn).await?;
        let judging = match (self.judge)() {
            Ok(Some(judge)) => {
                let judged = usize::try_from(self.source.judged_on(day).await?).unwrap_or(0);
                let left = judge.cap.saturating_sub(judged);
                Some(Judging {
                    judge,
                    left: AtomicUsize::new(left),
                })
            }
            Ok(None) => None,
            Err(e) => {
                tracing::error!(target: "shadow", error = %format!("{e:#}"), "artifacts' known items are not judged");
                None
            }
        };
        let asking = Asking {
            kernel,
            recorder: self.recorder.as_ref(),
            run,
            pace: self.settings.pace,
            permit_wait: self.permit_wait,
        };
        let (outcome, why, done) = ask_each(
            &asking,
            &items,
            go,
            |item, why| vec![not_asked(item, why)],
            |i| self.ask(&items[i], kernel.deadline(), judging.as_ref()),
        )
        .await?;
        Ok((outcome, why, drawn, done))
    }

    /// Draw from every artifact, a page at a time; `None` when `go` said
    /// to stop between two pages.
    async fn draw(
        &self,
        seed: u64,
        now: DateTime<Utc>,
        go: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<Option<(Vec<ArtifactItem>, KnownItemsDrawn)>> {
        let settle = chrono::Duration::from_std(self.settle)?;
        let mut draw = ArtifactDraw::new(seed, self.settings.items, now - settle);
        let mut after: Option<String> = None;
        loop {
            if !go() {
                return Ok(None);
            }
            let page = self.source.page(after.as_deref(), DRAW_PAGE).await?;
            for artifact in &page {
                draw.offer(artifact);
            }
            match page.last() {
                Some(last) if page.len() == DRAW_PAGE => after = Some(last.id.clone()),
                _ => break,
            }
            if !self.page_pace.is_zero() {
                tokio::time::sleep(self.page_pace).await;
            }
        }
        Ok(Some(draw.finish()))
    }

    /// `item` asked of both indexes, and judged while the judge has any
    /// left to judge.
    async fn ask(
        &self,
        item: &ArtifactItem,
        deadline: Duration,
        judging: Option<&Judging>,
    ) -> Vec<KnownItemRow> {
        let (primary, primary_ids) = side(self.serving.as_ref(), item, deadline).await;
        let (shadow, shadow_ids) = side(self.mirror.as_ref(), item, deadline).await;
        let (metrics, ndcg) = match (&primary_ids, &shadow_ids) {
            (Some(p), Some(s)) => {
                let metrics = RankMetrics::of(p, s, DEPTH, self.rbo_p);
                let ndcg = match judging {
                    Some(judging) if take_one(&judging.left) => {
                        match self.judged(&judging.judge, &item.query, p, s).await {
                            Ok(ndcg) => Some(ndcg),
                            Err(e) => {
                                tracing::warn!(target: "shadow", error = %format!("{e:#}"), artifact = %item.id, "a known item could not be judged");
                                None
                            }
                        }
                    }
                    _ => None,
                };
                (Some(metrics), ndcg)
            }
            _ => (None, None),
        };
        vec![KnownItemRow {
            item_id: item.id.clone(),
            alpha: 1.0,
            status: KnownItemStatus::Asked,
            primary,
            shadow,
            metrics,
            ndcg,
            population: Population::Scoped,
            mode: QueryMode::Window,
        }]
    }

    /// Each side's nDCG@5, by the judge's grades of the pool of both
    /// sides' first five (an artifact Postgres no longer holds gains
    /// nothing).
    async fn judged(
        &self,
        judge: &Judge,
        query: &str,
        primary: &[String],
        shadow: &[String],
    ) -> Result<(f64, f64)> {
        let depth = crate::known_item_judge::DEPTH;
        let mut seen = HashSet::new();
        let pool: Vec<String> = primary
            .iter()
            .take(depth)
            .chain(shadow.iter().take(depth))
            .filter(|id| seen.insert((*id).clone()))
            .cloned()
            .collect();
        let held = self
            .source
            .by_id(&pool, crate::known_item_judge::EXCERPT_CHARS)
            .await?;
        // Shown in the pool's order.
        let shown: Vec<ArtifactText> = pool
            .iter()
            .filter_map(|id| held.iter().find(|a| &a.id == id).cloned())
            .collect();
        let grades = judge.grades.grade(query, &shown).await?;
        let grades: HashMap<String, u8> = shown.iter().map(|a| a.id.clone()).zip(grades).collect();
        Ok((
            crate::known_item_judge::ndcg(primary, &grades, depth),
            crate::known_item_judge::ndcg(shadow, &grades, depth),
        ))
    }
}

/// Take one from `left`, if any is.
fn take_one(left: &AtomicUsize) -> bool {
    left.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
}

/// One index's answer to `item`: where the artifact came, and the ids
/// that did; or the error.
async fn side(
    index: &dyn ArtifactIndexer,
    item: &ArtifactItem,
    deadline: Duration,
) -> (KnownItemSide, Option<Vec<String>>) {
    let began = std::time::Instant::now();
    let answered = tokio::time::timeout(deadline, index.search(&item.query, DEPTH)).await;
    let ms = Some(began.elapsed().as_secs_f64() * 1000.0);
    match answered {
        Ok(Ok(found)) => {
            let mut seen = HashSet::new();
            let ids: Vec<String> = found
                .into_iter()
                .map(|(id, _)| id)
                .filter(|id| seen.insert(id.clone()))
                .take(DEPTH)
                .collect();
            let side = KnownItemSide {
                rank: ids.iter().position(|id| *id == item.id).map(|r| r + 1),
                by_duplicate: false,
                count: Some(ids.len()),
                ms,
                error: None,
            };
            (side, Some(ids))
        }
        Ok(Err(e)) => (
            KnownItemSide {
                ms,
                error: Some(format!("{e:#}")),
                ..KnownItemSide::default()
            },
            None,
        ),
        Err(_) => (
            KnownItemSide {
                ms,
                error: Some(format!("timed out after {deadline:?}")),
                ..KnownItemSide::default()
            },
            None,
        ),
    }
}

fn not_asked(item: &ArtifactItem, why: KnownItemStatus) -> KnownItemRow {
    KnownItemRow {
        item_id: item.id.clone(),
        alpha: 1.0,
        status: why,
        primary: KnownItemSide::default(),
        shadow: KnownItemSide::default(),
        metrics: None,
        ndcg: None,
        population: Population::Scoped,
        mode: QueryMode::Window,
    }
}

/// Start the artifacts' nightly known-item runs on the leader, when
/// `[artifacts] index_backend` shadows them and `items` is not 0; `shadow`
/// is the hub's shadow tables. Failing to start costs the shadow, never
/// the hub.
pub fn start(
    config: &workforce_common::settings::WorkforceConfig,
    shadow: Option<Arc<ShadowStore>>,
    leader: Option<Arc<std::sync::atomic::AtomicBool>>,
    drain: tokio_util::sync::CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    if config.artifacts.index_backend != workforce_common::settings::SHADOWED {
        return None;
    }
    match runner(config, shadow) {
        Ok(Some(runner)) => Some(crate::known_items::spawn(
            Arc::new(runner) as Arc<dyn Nightly>,
            Arc::new(std::sync::atomic::AtomicBool::new(
                config.vellev.shadow.paused,
            )),
            leader,
            drain,
        )),
        Ok(None) => None,
        Err(e) => {
            tracing::error!(target: "shadow", error = %format!("{e:#}"), "artifacts' known-item runs are not made");
            None
        }
    }
}

/// The runner `config` asks for: `None` when `items` is 0.
fn runner(
    config: &workforce_common::settings::WorkforceConfig,
    shadow: Option<Arc<ShadowStore>>,
) -> Result<Option<ArtifactRunner>> {
    use crate::artifact_index::{ArtifactIndex, VellevArtifacts, VellevPace, WeaviateArtifacts};
    use workforce_common::settings::ShadowedStore;
    let settings = config.vellev.shadow.known_items()?;
    if settings.items == 0 {
        return Ok(None);
    }
    let shadow = shadow.ok_or_else(|| eyre::eyre!("the shadow tables are not open"))?;
    let read = config.vellev.shadow.settings(ShadowedStore::Artifacts)?;
    let v = &config.vellev;
    let key = v
        .api_key
        .as_deref()
        .ok_or_else(|| eyre::eyre!("[vellev] api_key is not set"))?;
    // As the read shadow asks it: one attempt within the deadline (A7).
    let mirror = VellevArtifacts::paced(
        &v.url,
        key,
        &v.tenant,
        VellevPace::shadow_read(read.deadline),
    )?;
    let serving = WeaviateArtifacts::new(&config.weaviate.url, config.weaviate.api_key.as_deref());
    let judging = config.clone();
    Ok(Some(ArtifactRunner {
        source: Arc::clone(&shadow) as Arc<dyn Artifacts>,
        serving: Arc::new(ArtifactIndex::new(serving)),
        mirror: Arc::new(ArtifactIndex::new(mirror)),
        kernel: Arc::new(crate::serve::artifact_shadow),
        in_step: Arc::new(crate::serve::artifact_mirror_in_step),
        recorder: shadow,
        judge: Arc::new(move || Judge::from_config(&judging, crate::artifact_index::tagger_llm)),
        settings,
        rbo_p: read.rbo_p,
        settle: SETTLE,
        permit_wait: crate::known_items::PERMIT_WAIT,
        page_pace: crate::known_items::DRAW_PAGE_PACE,
    }))
}

#[cfg(test)]
#[path = "artifact_known_items_tests.rs"]
mod tests;

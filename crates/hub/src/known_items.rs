//! Shadow mode's known-item runs of memories (DW-676, PR 10; design r2
//! §6.4, A13).
//!
//! Live recalls are too few for a verdict (about 300 a week against the
//! 1,000 the exit criteria want), so the memory verdict rests on these
//! runs. Each night the leader draws memories from the Weaviate collection
//! that serves, makes a query of each, and asks both stores, Weaviate and
//! vellev's replica, at both alphas (1.0, the vector leg alone, and the
//! serving store's own), whether the memory comes back first.
//!
//! - **The draw** is seeded: a run's seed is made from
//!   `[vellev.shadow.known_items] seed` and the run's UTC day, and a
//!   memory is drawn when its key, a hash of the seed and its memory id,
//!   is among the `items` smallest. The same seed and the same memories
//!   give the same set, whatever order they are read in.
//! - **Two populations.** Memories filed under an identity are recalled
//!   within it, as an agent recalls them. Those filed under none (about a
//!   fifth of them) are drawn apart, up to `unscoped_items`, and recalled
//!   unscoped, as system jobs recall them. Each row says which.
//! - **The queries** are made from the memory, never its whole content.
//!   Each memory is asked in two modes: `window`, half its words
//!   ([`query_of`]), and `hard`, 4 to 8 from its middle
//!   ([`hard_query_of`]).
//! - **Found** means the memory came back by its id, *or* as another
//!   memory of the same content ([`normalise`]d alike on both sides): a
//!   query made of a memory finds its word-for-word duplicates as well as
//!   itself, and either is right. A find by a duplicate is recorded apart.
//! - **What is never drawn** is what a recall cannot return (deleted, or
//!   carrying an audit tag), each counted by why ([`Excluded`]); and a
//!   memory last written lately, which the replica may not hold yet.
//! - **Asking.** Each query is embedded once, and that vector handed to
//!   both stores at both alphas (A5), each call bounded by the memory
//!   shadow's deadline and labelled a shadow read for the audit (A8).
//!   The bare stores are asked, never the hub's shadowed store, whose
//!   recalls are the live samples: a run adds nothing to them.
//! - **Load.** One item at a time, `pace` apart, each holding one of the
//!   memory shadow's permits ([`Shadow::permit`]), so the live samples
//!   keep the rest; never while the shadow is paused; on the leader only,
//!   one run a day, started within its window, claimed in Postgres.
//!
//! **Limitation: these queries are easy.** A query is an excerpt of the
//! very memory it is to find, so it shares that memory's words, which the
//! hybrid's keyword leg rewards; a live recall rarely does. The runs
//! measure whether the two indexes agree and rank alike on such queries,
//! not how well either serves a real question. The `hard` mode narrows
//! the gap; it does not close it.
//!
//! What a run records (`shadow_known_item_runs`, `shadow_known_items`)
//! holds no query text, which is made again from the run's seed and the
//! memory, and no identity: a memory is named by its id alone.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Timelike, Utc};
use eyre::{eyre, Result};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use workforce_common::settings::KnownItemSettings;
use workforce_common::shadow::{RankMetrics, Shadow};
use workforce_common::shadow_postgres::{
    KnownItemClaim, KnownItemOutcome, KnownItemRow, KnownItemSide, KnownItemStatus,
    KnownItemsDrawn, Population, QueryMode, ShadowStore, KNOWN_ITEM_STALE,
};
use workforce_memory::admin::SyncSource;
use workforce_memory::embedding::EmbeddingBackend;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{MemoryFilter, MemorySearchResult};

/// The store a run is recorded under.
pub const STORE: &str = "memories";
/// How a query is made from a memory ([`query_of`]). Changed only with
/// that code, so that a recorded seed always means what it meant.
pub const DERIVATION: i32 = 1;
/// Fewest words a memory must have to be drawn.
pub const MIN_WORDS: usize = 8;
/// Fewest and most words a query takes.
pub const QUERY_WORDS: (usize, usize) = (6, 24);
/// Fewest and most words a hard query takes ([`hard_query_of`]).
pub const HARD_QUERY_WORDS: (usize, usize) = (4, 8);
/// How often the leader looks whether a night's run is due.
pub const CHECK_EVERY: Duration = Duration::from_secs(10 * 60);
/// How long an item waits for one of the shadow's permits before it is
/// recorded as having had none.
pub const PERMIT_WAIT: Duration = Duration::from_secs(30);
/// vellev, Weaviate or the embedder failing this many items in a row ends
/// a run: down, it would otherwise cost every item a deadline.
pub const VELLEV_DOWN_AFTER: usize = 20;
/// The pause between two pages of the draw: Weaviate serves users.
pub const DRAW_PAGE_PACE: Duration = Duration::from_millis(100);
/// Time for the leader election to be wired before the first look.
const LEADER_WIRING: Duration = Duration::from_secs(20);

/// Why a memory was not drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Excluded {
    /// Forgotten: no recall returns it.
    Deleted,
    /// Carrying a tag the audit writes, which recall leaves out.
    Polluted,
    /// Written within the settling time: the replica may not hold it yet.
    TooRecent,
    /// Fewer than [`MIN_WORDS`] words, or no content.
    TooShort,
    /// Its query holds nothing to embed.
    NotVectorizable,
    /// No memory id or object id: not a memory as stored.
    Malformed,
    /// An artifact still noted for the serving index or the mirror to
    /// take: one of them may not hold it as it is yet.
    Pending,
}

impl Excluded {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deleted => "deleted",
            Self::Polluted => "polluted",
            Self::TooRecent => "too_recent",
            Self::TooShort => "too_short",
            Self::NotVectorizable => "not_vectorizable",
            Self::Malformed => "malformed",
            Self::Pending => "pending",
        }
    }
}

/// A run's seed: `base` and the run's UTC `day`.
pub fn run_seed(base: u64, day: NaiveDate) -> u64 {
    let mut hashed = blake3::Hasher::new();
    hashed.update(b"dw676-known-items\0");
    hashed.update(&base.to_le_bytes());
    hashed.update(day.format("%Y-%m-%d").to_string().as_bytes());
    first_u64(hashed.finalize())
}

/// `memory_id`'s key in a run of `seed`, for `what`.
fn key(seed: u64, what: &[u8], memory_id: &str) -> u64 {
    let mut hashed = blake3::Hasher::new();
    hashed.update(&seed.to_le_bytes());
    hashed.update(what);
    hashed.update(&[0]);
    hashed.update(memory_id.as_bytes());
    first_u64(hashed.finalize())
}

fn first_u64(hash: blake3::Hash) -> u64 {
    let mut first = [0u8; 8];
    first.copy_from_slice(&hash.as_bytes()[..8]);
    u64::from_le_bytes(first)
}

/// The query made of `content`, the memory `memory_id`'s, for a run of
/// `seed`: a window of its words, never all of them; `None` when it has
/// fewer than [`MIN_WORDS`].
pub fn query_of(seed: u64, memory_id: &str, content: &str) -> Option<String> {
    let words: Vec<&str> = content.split_whitespace().collect();
    let n = words.len();
    if n < MIN_WORDS {
        return None;
    }
    // Half its words, rounded up, within QUERY_WORDS: from MIN_WORDS (8)
    // words on, always fewer than it has.
    let taken = n.div_ceil(2).clamp(QUERY_WORDS.0, QUERY_WORDS.1);
    let places = (n - taken + 1) as u64;
    let start = (key(seed, b"start", memory_id) % places) as usize;
    Some(words[start..start + taken].join(" "))
}

/// The query made of `content` in the hard mode: a few words (4 to 8, a
/// quarter of them) from the middle half of its words, placed by the
/// seed; `None` when it has fewer than [`MIN_WORDS`]. Harder than
/// [`query_of`]'s: fewer words to match, and never the start, which a
/// memory's own wording most often leads with.
pub fn hard_query_of(seed: u64, memory_id: &str, content: &str) -> Option<String> {
    let words: Vec<&str> = content.split_whitespace().collect();
    let n = words.len();
    if n < MIN_WORDS {
        return None;
    }
    let taken = n.div_ceil(4).clamp(HARD_QUERY_WORDS.0, HARD_QUERY_WORDS.1);
    let spare = n - taken;
    // Starts from a quarter of the spare words to three quarters.
    let (first, last) = (spare / 4, (3 * spare).div_ceil(4));
    let start = first + (key(seed, b"hard", memory_id) % (last - first + 1) as u64) as usize;
    Some(words[start..start + taken].join(" "))
}

/// Text as found and drawn are compared: words split on any whitespace,
/// joined by single spaces, lowercased. A hit whose content so equals
/// the drawn memory's is a duplicate of it, and counts as finding it.
pub fn normalise(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// One memory drawn, and the queries made of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub memory_id: String,
    /// The identity it is filed under; `None` for one filed under none,
    /// recalled unscoped.
    pub identity_id: Option<String>,
    /// Its content, [`normalise`]d.
    pub content: String,
    /// [`query_of`]'s query, and [`hard_query_of`]'s.
    pub query: String,
    pub hard_query: String,
}

impl Item {
    pub fn population(&self) -> Population {
        match self.identity_id {
            Some(_) => Population::Scoped,
            None => Population::Unscoped,
        }
    }
}

/// A draw under way: the objects offered so far, the `items` with the
/// smallest keys kept.
pub struct Draw {
    seed: u64,
    settled_before: DateTime<Utc>,
    /// By key and memory id, the smallest kept: a memory's key is its
    /// own, so two objects of it meet here, the smaller object id kept.
    /// Memories filed under an identity, and those under none, apart.
    scoped: Smallest<Kept>,
    unscoped: Smallest<Kept>,
    drawn: KnownItemsDrawn,
}

struct Kept {
    object_id: String,
    identity_id: Option<String>,
    content: String,
    query: String,
    hard_query: String,
}

impl Draw {
    /// A draw of `items` memories filed under an identity and
    /// `unscoped_items` under none, for a run of `seed`, of memories last
    /// written before `settled_before`.
    pub fn new(
        seed: u64,
        items: usize,
        unscoped_items: usize,
        settled_before: DateTime<Utc>,
    ) -> Self {
        Self {
            seed,
            settled_before,
            scoped: Smallest::new(items),
            unscoped: Smallest::new(unscoped_items),
            drawn: KnownItemsDrawn::default(),
        }
    }

    /// Offer one exported object (`{id, properties}`).
    pub fn offer(&mut self, object: &Value) {
        self.drawn.population += 1;
        let (memory_id, kept) = match self.eligible(object) {
            Ok(eligible) => eligible,
            Err(why) => {
                *self.drawn.excluded.entry(why.as_str()).or_default() += 1;
                return;
            }
        };
        let entry = (draw_key(self.seed, &memory_id), memory_id);
        let object_id = kept.object_id.clone();
        let pool = match kept.identity_id {
            Some(_) => &mut self.scoped,
            None => &mut self.unscoped,
        };
        pool.offer(entry, kept, |held| object_id < held.object_id);
    }

    /// `object`'s memory id and what is kept of it, or why it is not drawn.
    fn eligible(&self, object: &Value) -> Result<(String, Kept), Excluded> {
        let p = &object["properties"];
        let memory_id = p["memoryId"].as_str().filter(|m| !m.is_empty());
        let object_id = object["id"].as_str();
        let (Some(memory_id), Some(object_id)) = (memory_id, object_id) else {
            return Err(Excluded::Malformed);
        };
        if p["deleted"].as_bool() == Some(true) {
            return Err(Excluded::Deleted);
        }
        let polluted = p["tags"].as_array().is_some_and(|tags| {
            tags.iter().any(|t| {
                t.as_str()
                    .is_some_and(|t| workforce_memory::audit::POLLUTED_KIND_TAGS.contains(&t))
            })
        });
        if polluted {
            return Err(Excluded::Polluted);
        }
        let identity_id = p["identityId"]
            .as_str()
            .filter(|i| !i.is_empty())
            .map(String::from);
        // Last written: memories hold no `updatedAt` today, but a write
        // that comes again (`lastRecurrenceAt`) is one, and an `updatedAt`
        // is read should the schema gain one. A memory without a time of
        // writing is from before it was kept, and so long settled.
        let last_written = ["createdAt", "lastRecurrenceAt", "updatedAt"]
            .iter()
            .filter_map(|name| p[*name].as_str())
            .filter_map(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Utc))
            .max();
        if last_written.is_some_and(|at| at >= self.settled_before) {
            return Err(Excluded::TooRecent);
        }
        let content = p["content"].as_str().unwrap_or_default();
        let (Some(query), Some(hard_query)) = (
            query_of(self.seed, memory_id, content),
            hard_query_of(self.seed, memory_id, content),
        ) else {
            return Err(Excluded::TooShort);
        };
        let vectorizable =
            |q: &str| workforce_common::vectorizable::VectorizableText::try_new(q).is_ok();
        if !vectorizable(&query) || !vectorizable(&hard_query) {
            return Err(Excluded::NotVectorizable);
        }
        Ok((
            memory_id.to_string(),
            Kept {
                object_id: object_id.to_string(),
                identity_id,
                content: normalise(content),
                query,
                hard_query,
            },
        ))
    }

    /// The items drawn, in the order of their keys, and what was drawn.
    /// Both pools are asked in one order of keys, so a run stopped part
    /// way has asked of each alike.
    pub fn finish(mut self) -> (Vec<Item>, KnownItemsDrawn) {
        self.drawn.drawn_unscoped = self.unscoped.len();
        self.drawn.drawn = self.scoped.len() + self.drawn.drawn_unscoped;
        let mut kept: Vec<((u64, String), Kept)> = self
            .scoped
            .into_values()
            .chain(self.unscoped.into_values())
            .collect();
        kept.sort_by(|(a, _), (b, _)| a.cmp(b));
        let items = kept
            .into_iter()
            .map(|((_, memory_id), kept)| Item {
                memory_id,
                identity_id: kept.identity_id,
                content: kept.content,
                query: kept.query,
                hard_query: kept.hard_query,
            })
            .collect();
        (items, self.drawn)
    }
}

/// Where a run is recorded: [`ShadowStore`] in a hub.
#[async_trait]
pub trait Recorder: Send + Sync {
    async fn claim(&self, claim: &KnownItemClaim) -> Result<Option<i64>>;
    async fn drawn(&self, run: i64, drawn: &KnownItemsDrawn) -> Result<()>;
    async fn items(&self, run: i64, items: &[KnownItemRow]) -> Result<()>;
    async fn finish(
        &self,
        run: i64,
        outcome: KnownItemOutcome,
        error: Option<&str>,
    ) -> Result<bool>;
}

#[async_trait]
impl Recorder for ShadowStore {
    async fn claim(&self, claim: &KnownItemClaim) -> Result<Option<i64>> {
        self.claim_known_item_run(claim, KNOWN_ITEM_STALE).await
    }
    async fn drawn(&self, run: i64, drawn: &KnownItemsDrawn) -> Result<()> {
        self.record_known_items_drawn(run, drawn).await
    }
    async fn items(&self, run: i64, items: &[KnownItemRow]) -> Result<()> {
        self.record_known_items(run, items).await
    }
    async fn finish(
        &self,
        run: i64,
        outcome: KnownItemOutcome,
        error: Option<&str>,
    ) -> Result<bool> {
        self.finish_known_item_run(run, outcome, error).await
    }
}

/// A night's run, ready to be made.
pub struct Runner {
    /// Weaviate's collection that serves memories, which items are drawn
    /// from.
    pub source: Arc<dyn SyncSource>,
    /// The same collection, asked.
    pub serving: Arc<dyn MemoryStore>,
    /// vellev's replica, asked.
    pub replica: Arc<dyn MemoryStore>,
    /// The hub's embedder, whose vectors both hold.
    pub embedder: Arc<dyn EmbeddingBackend>,
    /// The memory shadow's kernel: its permits, and whether it is paused.
    pub kernel: Arc<Shadow>,
    pub recorder: Arc<dyn Recorder>,
    pub settings: KnownItemSettings,
    /// Depth each answer is read to, and RBO's p.
    pub k: usize,
    pub rbo_p: f64,
    /// Memories written within this long are not drawn.
    pub settle: Duration,
    /// How long an item waits for a permit.
    pub permit_wait: Duration,
    /// The pause between two pages of the draw.
    pub page_pace: Duration,
}

/// What a run came to.
#[derive(Debug, Clone, PartialEq)]
pub struct Ran {
    pub run: i64,
    pub outcome: KnownItemOutcome,
    pub error: Option<String>,
    pub drawn: KnownItemsDrawn,
    /// Items asked or recorded as not asked.
    pub items: usize,
}

impl Runner {
    /// Make `day`'s run, at `now`, while `go` says to go on (the leader,
    /// not paused, not draining). `Ok(None)` when the day is claimed
    /// already.
    pub async fn run(
        &self,
        day: NaiveDate,
        now: DateTime<Utc>,
        go: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<Option<Ran>> {
        if self.settings.items == 0 {
            return Ok(None);
        }
        let model = self.embedder.model_id();
        for (name, store) in [("Weaviate", &self.serving), ("vellev", &self.replica)] {
            let holds = store.query_model();
            if holds.as_deref() != Some(model) {
                return Err(eyre!(
                    "{name}'s store of memories does not hold vectors of the hub's model {model} (it holds {}): known items are not asked of it, as no vector is handed across models",
                    holds.as_deref().unwrap_or("none it names")
                ));
            }
        }
        let alpha = self
            .serving
            .recall_settings()
            .ok_or_else(|| eyre!("Weaviate's store of memories names no hybrid alpha"))?
            .alpha;
        if !self.kernel.enabled() {
            return Ok(None);
        }
        let seed = run_seed(self.settings.seed, day);
        let claim = KnownItemClaim {
            store: STORE.to_string(),
            run_day: day,
            base_seed: self.settings.seed,
            run_seed: seed,
            derivation: DERIVATION,
            k: self.k,
            alpha_serving: alpha,
            items_wanted: self.settings.items,
        };
        let Some(run) = self.recorder.claim(&claim).await? else {
            return Ok(None);
        };
        let made = self.made(run, seed, now, alpha, go).await;
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

    /// Run `run`, claimed: draw, then ask item after item.
    async fn made(
        &self,
        run: i64,
        seed: u64,
        now: DateTime<Utc>,
        alpha: f32,
        go: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<(KnownItemOutcome, Option<String>, KnownItemsDrawn, usize)> {
        let Some((items, drawn)) = self.draw(seed, now, go).await? else {
            return Ok((
                KnownItemOutcome::Incomplete,
                Some(STOPPED_IN_DRAW.to_string()),
                KnownItemsDrawn::default(),
                0,
            ));
        };
        self.recorder.drawn(run, &drawn).await?;
        let alphas: Vec<f32> = if alpha == 1.0 {
            vec![1.0]
        } else {
            vec![alpha, 1.0]
        };
        let asking = Asking {
            kernel: &self.kernel,
            recorder: self.recorder.as_ref(),
            run,
            pace: self.settings.pace,
            permit_wait: self.permit_wait,
        };
        let (outcome, why, done) = ask_each(
            &asking,
            &items,
            go,
            |item, why| not_asked(item, &alphas, why),
            |i| self.ask(&items[i], &alphas),
        )
        .await?;
        Ok((outcome, why, drawn, done))
    }

    /// Draw from every object of the serving collection, a page at a
    /// time; `None` when `go` said to stop between two pages.
    async fn draw(
        &self,
        seed: u64,
        now: DateTime<Utc>,
        go: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<Option<(Vec<Item>, KnownItemsDrawn)>> {
        let settle = chrono::Duration::from_std(self.settle)?;
        let mut draw = Draw::new(
            seed,
            self.settings.items,
            self.settings.unscoped_items,
            now - settle,
        );
        let mut after: Option<String> = None;
        loop {
            if !go() {
                return Ok(None);
            }
            let page = self.source.export_page(after.as_deref(), false).await?;
            for object in &page {
                draw.offer(object);
            }
            if page.len() < workforce_memory::admin::EXPORT_PAGE {
                break;
            }
            after = Some(
                page.last()
                    .and_then(|o| o["id"].as_str())
                    .ok_or_else(|| eyre!("an exported object has no id"))?
                    .to_string(),
            );
            if !self.page_pace.is_zero() {
                tokio::time::sleep(self.page_pace).await;
            }
        }
        Ok(Some(draw.finish()))
    }

    /// `item` asked in each mode: its query embedded once, and asked of
    /// both stores at each of `alphas`.
    async fn ask(&self, item: &Item, alphas: &[f32]) -> Vec<KnownItemRow> {
        let mut rows = Vec::with_capacity(2 * alphas.len());
        for mode in [QueryMode::Window, QueryMode::Hard] {
            rows.extend(self.ask_in(item, mode, alphas).await);
        }
        rows
    }

    async fn ask_in(&self, item: &Item, mode: QueryMode, alphas: &[f32]) -> Vec<KnownItemRow> {
        let query = match mode {
            QueryMode::Window => &item.query,
            QueryMode::Hard => &item.hard_query,
        };
        let deadline = self.kernel.deadline();
        let vector = match tokio::time::timeout(deadline, self.embedder.embed_query(query)).await {
            Ok(Ok(vector)) => vector,
            _ => return not_asked_in(item, mode, alphas, KnownItemStatus::EmbedFailed),
        };
        let mut rows = Vec::with_capacity(alphas.len());
        for &alpha in alphas {
            // As a recall of the memory's identity is made; or, for one
            // filed under none, as a system job's, across every identity.
            let filter = MemoryFilter {
                identity_id: item.identity_id.clone(),
                system_unscoped: item.identity_id.is_none(),
                limit: Some(self.k),
                hybrid_alpha: Some(alpha),
                ..MemoryFilter::default()
            };
            let (primary, primary_ids) = self
                .side(self.serving.as_ref(), item, query, &vector, filter.clone())
                .await;
            let (shadow, shadow_ids) = self
                .side(self.replica.as_ref(), item, query, &vector, filter)
                .await;
            let metrics = match (primary_ids, shadow_ids) {
                (Some(p), Some(s)) => Some(RankMetrics::of(&p, &s, self.k, self.rbo_p)),
                _ => None,
            };
            rows.push(KnownItemRow {
                item_id: item.memory_id.clone(),
                alpha,
                status: KnownItemStatus::Asked,
                primary,
                shadow,
                metrics,
                ndcg: None,
                population: item.population(),
                mode,
            });
        }
        rows
    }

    /// One store's answer to `item`, asked by `query`: where the memory
    /// came back, by its id or as a duplicate (the same content,
    /// [`normalise`]d), and the memories that did; or the error.
    async fn side(
        &self,
        store: &dyn MemoryStore,
        item: &Item,
        query: &str,
        vector: &[f32],
        filter: MemoryFilter,
    ) -> (KnownItemSide, Option<Vec<String>>) {
        let deadline = self.kernel.deadline();
        let began = std::time::Instant::now();
        let answered = tokio::time::timeout(
            deadline,
            workforce_memory::access_audit::as_shadow(store.recall_with_vector(
                query,
                Some(vector.to_vec()),
                filter,
            )),
        )
        .await;
        let ms = Some(began.elapsed().as_secs_f64() * 1000.0);
        match answered {
            Ok(Ok(hits)) => {
                let hits = distinct(&hits, self.k);
                let found = hits.iter().position(|hit| {
                    hit.memory.id.0 == item.memory_id
                        || normalise(&hit.memory.content) == item.content
                });
                let side = KnownItemSide {
                    rank: found.map(|r| r + 1),
                    by_duplicate: found.is_some_and(|r| hits[r].memory.id.0 != item.memory_id),
                    count: Some(hits.len()),
                    ms,
                    error: None,
                };
                let ids = hits.iter().map(|hit| hit.memory.id.0.clone()).collect();
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
}

/// `item` in each mode at each of `alphas`, not asked, for `why`.
fn not_asked(item: &Item, alphas: &[f32], why: KnownItemStatus) -> Vec<KnownItemRow> {
    [QueryMode::Window, QueryMode::Hard]
        .into_iter()
        .flat_map(|mode| not_asked_in(item, mode, alphas, why))
        .collect()
}

fn not_asked_in(
    item: &Item,
    mode: QueryMode,
    alphas: &[f32],
    why: KnownItemStatus,
) -> Vec<KnownItemRow> {
    alphas
        .iter()
        .map(|&alpha| KnownItemRow {
            item_id: item.memory_id.clone(),
            alpha,
            status: why,
            primary: KnownItemSide::default(),
            shadow: KnownItemSide::default(),
            metrics: None,
            ndcg: None,
            population: item.population(),
            mode,
        })
        .collect()
}

/// Why a run stopped while it drew.
pub(crate) const STOPPED_IN_DRAW: &str =
    "stopped while drawing: the shadow paused, the hub draining, or no longer the leader";

/// What [`ask_each`] needs of a run.
pub(crate) struct Asking<'a> {
    pub kernel: &'a Shadow,
    pub recorder: &'a dyn Recorder,
    pub run: i64,
    /// The pause between two items.
    pub pace: Duration,
    /// How long an item waits for a permit.
    pub permit_wait: Duration,
}

/// Ask `items` one after another, `pace` apart, each holding one of the
/// kernel's permits, while `go` says to go on; record what `ask` makes
/// of each, by its place in `items` (or `not_asked`'s rows, when it had no permit) in batches.
/// Returns how the run ended, why it stopped if it did, and how many items
/// were asked or recorded as not asked.
///
/// It stops part way when `go` says to, when the kernel is paused, when
/// vellev, Weaviate or the embedder fails [`VELLEV_DOWN_AFTER`] items in a
/// row, or when as many in a row find no permit.
pub(crate) async fn ask_each<T, A, Fut>(
    asking: &Asking<'_>,
    items: &[T],
    go: &(dyn Fn() -> bool + Send + Sync),
    not_asked: impl Fn(&T, KnownItemStatus) -> Vec<KnownItemRow>,
    mut ask: A,
) -> Result<(KnownItemOutcome, Option<String>, usize)>
where
    A: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = Vec<KnownItemRow>>,
{
    let mut rows: Vec<KnownItemRow> = Vec::new();
    let mut done = 0;
    let (mut vellev_failed, mut weaviate_failed, mut unembedded, mut no_permit) = (0, 0, 0, 0);
    let mut stopped = None;
    for (i, item) in items.iter().enumerate() {
        if !go() {
            stopped = Some(
                "stopped part way: the shadow paused, the hub draining, or no longer the leader"
                    .to_string(),
            );
            break;
        }
        if i > 0 && !asking.pace.is_zero() {
            tokio::time::sleep(asking.pace).await;
        }
        let permit = match tokio::time::timeout(asking.permit_wait, asking.kernel.permit()).await {
            Ok(Some(permit)) => permit,
            Ok(None) => {
                stopped = Some("stopped part way: the shadow paused".to_string());
                break;
            }
            Err(_) => {
                rows.extend(not_asked(item, KnownItemStatus::NoPermit));
                done += 1;
                no_permit += 1;
                if no_permit >= VELLEV_DOWN_AFTER {
                    stopped = Some(format!(
                        "stopped part way: the shadow's permits stayed taken for {no_permit} items in a row"
                    ));
                    break;
                }
                continue;
            }
        };
        no_permit = 0;
        let asked = ask(i).await;
        drop(permit);
        done += 1;
        // Each kind of failure is counted in a row, apart: one that keeps
        // failing ends the run, so that a store or embedder that is down
        // is not asked item after item, each to its deadline. Weaviate
        // serves users, and the embedder may cost per call.
        let failing = |counter: &mut usize, failed: bool| {
            *counter = if failed { *counter + 1 } else { 0 };
        };
        failing(
            &mut unembedded,
            asked
                .iter()
                .any(|row| row.status == KnownItemStatus::EmbedFailed),
        );
        if asked.iter().any(|row| row.status == KnownItemStatus::Asked) {
            failing(
                &mut vellev_failed,
                asked.iter().any(|row| row.shadow.error.is_some()),
            );
            failing(
                &mut weaviate_failed,
                asked.iter().any(|row| row.primary.error.is_some()),
            );
        }
        rows.extend(asked);
        if rows.len() >= workforce_common::shadow_postgres::SINK_BATCH {
            asking.recorder.items(asking.run, &rows).await?;
            rows.clear();
        }
        let down = [
            ("vellev failed", vellev_failed),
            ("Weaviate failed", weaviate_failed),
            ("the hub's embedder failed", unembedded),
        ]
        .into_iter()
        .find(|(_, n)| *n >= VELLEV_DOWN_AFTER);
        if let Some((what, n)) = down {
            stopped = Some(format!("stopped part way: {what} {n} items in a row"));
            break;
        }
    }
    if !rows.is_empty() {
        asking.recorder.items(asking.run, &rows).await?;
    }
    Ok(match stopped {
        None => (KnownItemOutcome::Ok, None, done),
        Some(why) => (KnownItemOutcome::Incomplete, Some(why), done),
    })
}

/// The `n` entries with the smallest `(key, id)` offered, each id once.
pub(crate) struct Smallest<V> {
    n: usize,
    kept: BTreeMap<(u64, String), V>,
}

impl<V> Smallest<V> {
    pub(crate) fn new(n: usize) -> Self {
        Self {
            n,
            kept: BTreeMap::new(),
        }
    }

    /// Offer `value` under `(key, id)`. One held already under it is
    /// replaced only when `replaces` says so of it: a key is its id's own,
    /// so a second offer is the same item again, from another place.
    pub(crate) fn offer(
        &mut self,
        entry: (u64, String),
        value: V,
        replaces: impl FnOnce(&V) -> bool,
    ) {
        if self.n == 0 {
            return;
        }
        if let Some(held) = self.kept.get_mut(&entry) {
            if replaces(held) {
                *held = value;
            }
            return;
        }
        if self.kept.len() < self.n {
            self.kept.insert(entry, value);
        } else if self
            .kept
            .last_key_value()
            .is_some_and(|(last, _)| entry < *last)
        {
            self.kept.pop_last();
            self.kept.insert(entry, value);
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.kept.len()
    }

    /// What was kept, in order of key.
    pub(crate) fn into_values(self) -> impl Iterator<Item = ((u64, String), V)> {
        self.kept.into_iter()
    }
}

/// The key `id` is drawn by in a run of `seed`.
pub(crate) fn draw_key(seed: u64, id: &str) -> u64 {
    key(seed, b"draw", id)
}

/// A night's run of one store, as [`spawn`] makes it.
#[async_trait]
pub trait Nightly: Send + Sync {
    /// The store its runs are recorded under.
    fn store(&self) -> &'static str;
    /// The UTC hours from which, and before which, a night's run starts.
    fn window_utc(&self) -> (u32, u32);
    /// Make `day`'s run at `now` while `go` says to; `Ok(None)` when none
    /// is made (claimed already, paused, or not ready to be compared).
    async fn run(
        &self,
        day: NaiveDate,
        now: DateTime<Utc>,
        go: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<Option<Ran>>;
}

#[async_trait]
impl Nightly for Runner {
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
        Runner::run(self, day, now, go).await
    }
}

/// Whether a night's run is due at `now`: from `hour_utc` on, on the
/// leader, while not paused.
pub fn due(
    now: DateTime<Utc>,
    hour_utc: u32,
    until_hour_utc: u32,
    leader: bool,
    paused: bool,
) -> bool {
    leader && !paused && (hour_utc..until_hour_utc).contains(&now.hour())
}

/// Look every [`CHECK_EVERY`] whether a run is due, and make it, until
/// `drain`. Each day is claimed in Postgres, so a run made is not made
/// again however often this looks.
pub fn spawn(
    runner: Arc<dyn Nightly>,
    paused: Arc<AtomicBool>,
    leader: Option<Arc<AtomicBool>>,
    drain: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            _ = drain.cancelled() => return,
            _ = tokio::time::sleep(LEADER_WIRING) => {}
        }
        let leads = {
            let leader = leader.clone();
            move || leader.as_ref().is_none_or(|l| l.load(Ordering::SeqCst))
        };
        let mut ticks = tokio::time::interval(CHECK_EVERY);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = drain.cancelled() => return,
                _ = ticks.tick() => {}
            }
            let now = Utc::now();
            let (from, until) = runner.window_utc();
            if !due(now, from, until, leads(), paused.load(Ordering::SeqCst)) {
                continue;
            }
            let go = {
                let (paused, drain, leads) = (Arc::clone(&paused), drain.clone(), leads.clone());
                move || !paused.load(Ordering::SeqCst) && !drain.is_cancelled() && leads()
            };
            match runner.run(now.date_naive(), now, &go).await {
                Ok(Some(ran)) => tracing::info!(
                    target: "shadow",
                    run = ran.run,
                    outcome = ran.outcome.as_str(),
                    drawn = ran.drawn.drawn,
                    items = ran.items,
                    error = ran.error.as_deref().unwrap_or_default(),
                    store = runner.store(),
                    "known-item run made"
                ),
                Ok(None) => {}
                Err(e) => tracing::error!(
                    target: "shadow",
                    error = %format!("{e:#}"),
                    store = runner.store(),
                    "a known-item run could not be made"
                ),
            }
        }
    })
}

/// A recall's answer as memories, each once, in the order given, the
/// first `k`.
fn distinct(hits: &[MemorySearchResult], k: usize) -> Vec<&MemorySearchResult> {
    let mut seen = std::collections::HashSet::new();
    hits.iter()
        .filter(|hit| seen.insert(hit.memory.id.0.clone()))
        .take(k)
        .collect()
}

#[cfg(test)]
#[path = "known_items_tests.rs"]
mod tests;

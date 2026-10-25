//! Where shadow mode's known-item runs are kept (DW-676, PR 10): the tables
//! of `V032__shadow_known_items.sql`.
//!
//! A run is claimed before it starts, one a store and UTC day, so that
//! neither two hubs nor a leader that changes run a day twice
//! ([`ShadowStore::claim_known_item_run`]). Its items are written as they
//! are asked, and [`ShadowStore::known_item_summary`] gives what a verdict
//! is worked out from: per alpha, how often each side put the memory first
//! over the items both answered, and the pairs on which they differ.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use eyre::{Context, Result};

use super::{block_on, bounded, bounded_query, ShadowStore, RECORD_LIMIT, SINK_BATCH};
use crate::shadow::RankMetrics;

/// Days a known-item run, and its items, are kept.
pub const KNOWN_ITEMS_RETENTION_DAYS: u32 = 180;
pub use crate::settings::KNOWN_ITEM_STALE;
/// Runs a store and day may be started, whatever came of them: a day that
/// keeps ending early (vellev down) is not tried all day.
pub const KNOWN_ITEM_ATTEMPTS: i64 = 3;

/// A run about to start, as claimed.
#[derive(Debug, Clone, PartialEq)]
pub struct KnownItemClaim {
    pub store: String,
    pub run_day: NaiveDate,
    pub base_seed: u64,
    pub run_seed: u64,
    pub derivation: i32,
    pub k: usize,
    pub alpha_serving: f32,
    pub items_wanted: usize,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownItemOutcome {
    /// Every item drawn was asked.
    Ok,
    /// Stopped part way; the items asked are kept.
    Incomplete,
    /// Nothing could be drawn.
    Failed,
}

impl KnownItemOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Incomplete => "incomplete",
            Self::Failed => "failed",
        }
    }
}

/// What was drawn: the memories looked at, those left out by reason, and
/// those drawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnownItemsDrawn {
    pub population: usize,
    pub excluded: BTreeMap<&'static str, usize>,
    pub drawn: usize,
    /// Of `drawn`, memories filed under no identity.
    pub drawn_unscoped: usize,
}

/// Which items a row is of.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Population {
    /// Recalled within the memory's identity; and every artifact.
    #[default]
    Scoped,
    /// Memories filed under no identity, recalled unscoped, as system jobs
    /// recall them.
    Unscoped,
}

impl Population {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scoped => "scoped",
            Self::Unscoped => "unscoped",
        }
    }

    fn parse(s: &str) -> Self {
        if s == "unscoped" {
            Self::Unscoped
        } else {
            Self::Scoped
        }
    }
}

/// How an item's query was made.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum QueryMode {
    /// Half its words.
    #[default]
    Window,
    /// A few words from its middle: harder to find.
    Hard,
}

impl QueryMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Hard => "hard",
        }
    }

    fn parse(s: &str) -> Self {
        if s == "hard" {
            Self::Hard
        } else {
            Self::Window
        }
    }
}

/// Whether an item was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownItemStatus {
    Asked,
    /// Its query could not be embedded; neither side was asked.
    EmbedFailed,
    /// The shadow's permits stayed taken.
    NoPermit,
}

impl KnownItemStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Asked => "asked",
            Self::EmbedFailed => "embed_failed",
            Self::NoPermit => "no_permit",
        }
    }
}

/// One side's answer to an item: where the memory came back (1 is first;
/// `None`, not within k), how many came back and how long the index call
/// took; or the error in their place. An error is never a miss.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KnownItemSide {
    pub rank: Option<usize>,
    /// Found at `rank` as another memory of the same content, not by its
    /// own id.
    pub by_duplicate: bool,
    pub count: Option<usize>,
    pub ms: Option<f64>,
    pub error: Option<String>,
}

/// One item at one alpha, as recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct KnownItemRow {
    /// A memory's id, or an artifact's.
    pub item_id: String,
    pub alpha: f32,
    pub status: KnownItemStatus,
    pub primary: KnownItemSide,
    pub shadow: KnownItemSide,
    /// The two answers compared, when both came.
    pub metrics: Option<RankMetrics>,
    /// The judge's nDCG@5 of each side, when it judged the item.
    pub ndcg: Option<(f64, f64)>,
    pub population: Population,
    pub mode: QueryMode,
}

/// One artifact as a draw or the judge reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactText {
    pub id: String,
    pub title: String,
    pub content: String,
    pub updated_at: DateTime<Utc>,
    /// Still noted for the serving index or the mirror to take.
    pub pending: bool,
}

/// A run's items at one alpha, summed. The rates, the reciprocal ranks and
/// the pairs are over the items both sides answered (`both_answered`), so
/// the two are compared on the same questions; errors are counted apart.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KnownItemSummary {
    pub population: Population,
    pub mode: QueryMode,
    pub alpha: f32,
    pub items: i64,
    pub asked: i64,
    pub embed_failed: i64,
    pub no_permit: i64,
    pub primary_errors: i64,
    pub shadow_errors: i64,
    pub both_answered: i64,
    /// The memory first, on each side.
    pub primary_first: i64,
    pub shadow_first: i64,
    /// First on Weaviate and not on vellev (McNemar's b), and the converse
    /// (c).
    pub only_primary_first: i64,
    pub only_shadow_first: i64,
    /// The memory within k, on each side.
    pub primary_within_k: i64,
    pub shadow_within_k: i64,
    /// Mean reciprocal rank, a miss counting 0.
    pub primary_mrr: Option<f64>,
    pub shadow_mrr: Option<f64>,
    pub mean_overlap: Option<f64>,
    pub mean_rbo: Option<f64>,
    pub mean_jaccard: Option<f64>,
    /// How often the two sides' first answers were the same memory, M2's
    /// top-1 (not either side finding the item first).
    pub top1_rate: Option<f64>,
    /// Items within the first 5, on each side (artifacts' recall@5).
    pub primary_within_5: i64,
    pub shadow_within_5: i64,
    /// Items the judge graded, and each side's mean nDCG@5 over them.
    pub judged: i64,
    pub primary_ndcg: Option<f64>,
    pub shadow_ndcg: Option<f64>,
    /// Of the finds within k, those by a duplicate, on each side.
    pub primary_found_by_duplicate: i64,
    pub shadow_found_by_duplicate: i64,
}

impl ShadowStore {
    /// Claim `claim`'s store and day for a run: first taking any run of the
    /// store still running after `stale` for abandoned. Returns the run's
    /// id, or `None` when the day is taken (a run of it is under way or
    /// ran to its end) or has been tried [`KNOWN_ITEM_ATTEMPTS`] times.
    pub async fn claim_known_item_run(
        &self,
        claim: &KnownItemClaim,
        stale: Duration,
    ) -> Result<Option<i64>> {
        let base_seed = i64::try_from(claim.base_seed)
            .context("a known-item run's base seed must fit in a BIGINT")?;
        let run_seed = format!("{:016x}", claim.run_seed);
        let derivation = claim.derivation;
        let k = i32::try_from(claim.k)?;
        let items_wanted = i32::try_from(claim.items_wanted)?;
        let stale_secs = stale.as_secs_f64();
        let ms = RECORD_LIMIT.as_millis().max(1);
        let run = async {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            tx.batch_execute(&format!(
                "SET LOCAL statement_timeout = {ms}; SET LOCAL lock_timeout = {ms}"
            ))
            .await?;
            tx.execute(
                "UPDATE shadow_known_item_runs
                 SET outcome = 'abandoned', finished_at = now(),
                     error = 'still running long after it started: its hub is taken to have gone'
                 WHERE store = $1 AND outcome = 'running'
                   AND started_at < now() - make_interval(secs => $2)",
                &[&claim.store, &stale_secs],
            )
            .await?;
            // The partial unique index holds one running or finished run a
            // day, whoever claims at the same time; the count bounds the
            // tries.
            let claimed = tx
                .query_opt(
                    "INSERT INTO shadow_known_item_runs
                         (store, run_day, base_seed, run_seed, derivation, k, alpha_serving,
                          items_wanted)
                     SELECT $1, $2, $3, $4, $5, $6, $7, $8
                     WHERE (SELECT count(*) FROM shadow_known_item_runs
                            WHERE store = $1 AND run_day = $2) < $9
                     ON CONFLICT DO NOTHING
                     RETURNING id",
                    &[
                        &claim.store,
                        &claim.run_day,
                        &base_seed,
                        &run_seed,
                        &derivation,
                        &k,
                        &claim.alpha_serving,
                        &items_wanted,
                        &KNOWN_ITEM_ATTEMPTS,
                    ],
                )
                .await?
                .map(|row| row.get::<_, i64>(0));
            tx.commit().await?;
            eyre::Ok(claimed)
        };
        tokio::time::timeout(RECORD_LIMIT * 2 + Duration::from_secs(1), run)
            .await
            .map_err(|_| eyre::eyre!("Postgres did not answer within {:?}", RECORD_LIMIT * 2))?
            .context("claiming a known-item run")
    }

    /// Record what run `run` drew.
    pub async fn record_known_items_drawn(&self, run: i64, drawn: &KnownItemsDrawn) -> Result<()> {
        let excluded = serde_json::Value::Object(
            drawn
                .excluded
                .iter()
                .map(|(reason, n)| (reason.to_string(), serde_json::Value::from(*n)))
                .collect(),
        );
        bounded(
            &self.pool,
            RECORD_LIMIT,
            "UPDATE shadow_known_item_runs
             SET population = $2, excluded = $3, drawn = $4, drawn_unscoped = $5
             WHERE id = $1",
            &[
                &run,
                &i32::try_from(drawn.population)?,
                &excluded,
                &i32::try_from(drawn.drawn)?,
                &i32::try_from(drawn.drawn_unscoped)?,
            ],
        )
        .await
        .context("recording what a known-item run drew")?;
        Ok(())
    }

    /// Record `items` of run `run`, [`SINK_BATCH`] to an insert.
    pub async fn record_known_items(&self, run: i64, items: &[KnownItemRow]) -> Result<()> {
        for batch in items.chunks(SINK_BATCH) {
            let rows: Vec<ItemRow> = batch.iter().map(|item| ItemRow::of(run, item)).collect();
            let placeholders: Vec<String> = (0..rows.len())
                .map(|r| {
                    let cells: Vec<String> = (1..=ITEM_WIDTH)
                        .map(|c| format!("${}", r * ITEM_WIDTH + c))
                        .collect();
                    format!("({})", cells.join(", "))
                })
                .collect();
            let sql = format!(
                "INSERT INTO shadow_known_items ({ITEM_COLUMNS}) VALUES {}",
                placeholders.join(", ")
            );
            let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                rows.iter().flat_map(|row| row.values()).collect();
            bounded(&self.pool, RECORD_LIMIT, &sql, &params)
                .await
                .context("recording known items")?;
        }
        Ok(())
    }

    /// End run `run` as `outcome`. Returns whether it was still running: a
    /// run taken for abandoned stays so.
    pub async fn finish_known_item_run(
        &self,
        run: i64,
        outcome: KnownItemOutcome,
        error: Option<&str>,
    ) -> Result<bool> {
        let touched = bounded(
            &self.pool,
            RECORD_LIMIT,
            "UPDATE shadow_known_item_runs SET outcome = $2, error = $3, finished_at = now()
             WHERE id = $1 AND outcome = 'running'",
            &[&run, &outcome.as_str(), &error],
        )
        .await
        .context("finishing a known-item run")?;
        Ok(touched == 1)
    }

    /// Run `run`'s items summed, per alpha, in ascending order of alpha.
    pub async fn known_item_summary(&self, run: i64) -> Result<Vec<KnownItemSummary>> {
        let rows = bounded_query(
            &self.pool,
            RECORD_LIMIT,
            "WITH i AS (
                 SELECT *, (status = 'asked' AND primary_error IS NULL
                            AND shadow_error IS NULL) AS both_answered
                 FROM shadow_known_items WHERE run_id = $1)
             SELECT population, mode, alpha,
                 count(*),
                 count(*) FILTER (WHERE status = 'asked'),
                 count(*) FILTER (WHERE status = 'embed_failed'),
                 count(*) FILTER (WHERE status = 'no_permit'),
                 count(*) FILTER (WHERE status = 'asked' AND primary_error IS NOT NULL),
                 count(*) FILTER (WHERE status = 'asked' AND shadow_error IS NOT NULL),
                 count(*) FILTER (WHERE both_answered),
                 count(*) FILTER (WHERE both_answered AND primary_rank = 1),
                 count(*) FILTER (WHERE both_answered AND shadow_rank = 1),
                 count(*) FILTER (WHERE both_answered AND primary_rank = 1
                                  AND shadow_rank IS DISTINCT FROM 1),
                 count(*) FILTER (WHERE both_answered AND shadow_rank = 1
                                  AND primary_rank IS DISTINCT FROM 1),
                 count(*) FILTER (WHERE both_answered AND primary_rank IS NOT NULL),
                 count(*) FILTER (WHERE both_answered AND shadow_rank IS NOT NULL),
                 avg(COALESCE(1.0 / primary_rank, 0)::float8) FILTER (WHERE both_answered),
                 avg(COALESCE(1.0 / shadow_rank, 0)::float8) FILTER (WHERE both_answered),
                 avg(overlap) FILTER (WHERE both_answered),
                 avg(rbo) FILTER (WHERE both_answered),
                 avg(jaccard) FILTER (WHERE both_answered),
                 (avg(CASE WHEN top1 THEN 1.0 ELSE 0.0 END) FILTER (WHERE both_answered))::float8,
                 count(*) FILTER (WHERE both_answered AND primary_rank <= 5),
                 count(*) FILTER (WHERE both_answered AND shadow_rank <= 5),
                 count(*) FILTER (WHERE both_answered AND primary_ndcg IS NOT NULL),
                 avg(primary_ndcg) FILTER (WHERE both_answered),
                 avg(shadow_ndcg) FILTER (WHERE both_answered),
                 count(*) FILTER (WHERE both_answered AND primary_rank IS NOT NULL
                                  AND primary_by_duplicate),
                 count(*) FILTER (WHERE both_answered AND shadow_rank IS NOT NULL
                                  AND shadow_by_duplicate)
             FROM i GROUP BY population, mode, alpha
             ORDER BY population, mode DESC, alpha",
            &[&run],
        )
        .await
        .context("summing a known-item run")?;
        Ok(rows
            .iter()
            .map(|r| KnownItemSummary {
                population: Population::parse(r.get(0)),
                mode: QueryMode::parse(r.get(1)),
                alpha: r.get(2),
                items: r.get(3),
                asked: r.get(4),
                embed_failed: r.get(5),
                no_permit: r.get(6),
                primary_errors: r.get(7),
                shadow_errors: r.get(8),
                both_answered: r.get(9),
                primary_first: r.get(10),
                shadow_first: r.get(11),
                only_primary_first: r.get(12),
                only_shadow_first: r.get(13),
                primary_within_k: r.get(14),
                shadow_within_k: r.get(15),
                primary_mrr: r.get(16),
                shadow_mrr: r.get(17),
                mean_overlap: r.get(18),
                mean_rbo: r.get(19),
                mean_jaccard: r.get(20),
                top1_rate: r.get(21),
                primary_within_5: r.get(22),
                shadow_within_5: r.get(23),
                judged: r.get(24),
                primary_ndcg: r.get(25),
                shadow_ndcg: r.get(26),
                primary_found_by_duplicate: r.get(27),
                shadow_found_by_duplicate: r.get(28),
            })
            .collect())
    }

    /// Up to `limit` artifacts whose ids come after `after`, in order of
    /// id: what a draw of artifacts reads, a page at a time.
    pub async fn artifact_page(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ArtifactText>> {
        let limit = i64::try_from(limit)?;
        let rows = bounded_query(
            &self.pool,
            RECORD_LIMIT,
            "SELECT a.id, a.title, a.content, a.updated_at,
                 EXISTS (SELECT 1 FROM artifact_index_pending p WHERE p.artifact_id = a.id)
                 -- V029 makes the mirror's list in the same migrations.
                 OR EXISTS (SELECT 1 FROM artifact_index_pending_mirror m
                            WHERE m.artifact_id = a.id)
             FROM artifacts a
             WHERE $1::text IS NULL OR a.id > $1
             ORDER BY a.id LIMIT $2",
            &[&after, &limit],
        )
        .await
        .context("reading a page of artifacts")?;
        Ok(rows.iter().map(artifact_text).collect())
    }

    /// The artifacts with these ids, each content cut to its first
    /// `chars` characters; an id held by none is left out.
    pub async fn artifacts_by_id(&self, ids: &[String], chars: usize) -> Result<Vec<ArtifactText>> {
        let chars = i32::try_from(chars)?;
        let rows = bounded_query(
            &self.pool,
            RECORD_LIMIT,
            "SELECT id, title, left(content, $2), updated_at, false FROM artifacts
             WHERE id = ANY($1)",
            &[&ids, &chars],
        )
        .await
        .context("reading artifacts by id")?;
        Ok(rows.iter().map(artifact_text).collect())
    }

    /// Items of `store` the judge graded in the runs for `day`.
    pub async fn judged_on(&self, store: &str, day: NaiveDate) -> Result<i64> {
        let rows = bounded_query(
            &self.pool,
            RECORD_LIMIT,
            "SELECT count(*) FROM shadow_known_items i
             JOIN shadow_known_item_runs r ON r.id = i.run_id
             WHERE r.store = $1 AND r.run_day = $2 AND i.primary_ndcg IS NOT NULL",
            &[&store, &day],
        )
        .await
        .context("counting the items judged today")?;
        Ok(rows.first().map(|r| r.get(0)).unwrap_or(0))
    }

    /// The known-item runs' retention pass: runs started before `before`
    /// deleted, with their items. Returns how many runs.
    pub fn prune_known_item_runs(&self, before: DateTime<Utc>) -> Result<usize> {
        // A run holds a night's items, a few thousand rows: a batch of runs
        // is kept small so one statement stays well within its limit.
        let batch = 10_i64;
        let mut removed = 0;
        loop {
            let deleted = block_on(async {
                Ok(bounded(
                    &self.pool,
                    self.retention_limit,
                    "DELETE FROM shadow_known_item_runs WHERE id IN (
                         SELECT id FROM shadow_known_item_runs WHERE started_at < $1
                         ORDER BY started_at LIMIT $2 FOR UPDATE SKIP LOCKED)",
                    &[&before, &batch],
                )
                .await? as usize)
            })?;
            removed += deleted;
            if deleted < batch as usize || removed >= MOST_PRUNED_A_PASS {
                return Ok(removed);
            }
        }
    }
}

fn artifact_text(row: &tokio_postgres::Row) -> ArtifactText {
    ArtifactText {
        id: row.get(0),
        title: row.get(1),
        content: row.get(2),
        updated_at: row.get(3),
        pending: row.get(4),
    }
}

/// Runs one retention pass deletes at most; the rest wait for the next.
const MOST_PRUNED_A_PASS: usize = 400;

const ITEM_COLUMNS: &str = "run_id, item_id, alpha, status, \
     primary_rank, primary_count, primary_ms, primary_error, \
     shadow_rank, shadow_count, shadow_ms, shadow_error, \
     overlap, jaccard, rbo, top1, primary_ndcg, shadow_ndcg, \
     population, mode, primary_by_duplicate, shadow_by_duplicate";
const ITEM_WIDTH: usize = 22;

/// One [`KnownItemRow`] as its columns hold it.
struct ItemRow {
    run: i64,
    item_id: String,
    alpha: f32,
    status: &'static str,
    primary: SideRow,
    shadow: SideRow,
    overlap: Option<f64>,
    jaccard: Option<f64>,
    rbo: Option<f64>,
    top1: Option<bool>,
    primary_ndcg: Option<f64>,
    shadow_ndcg: Option<f64>,
    population: &'static str,
    mode: &'static str,
}

struct SideRow {
    rank: Option<i32>,
    count: Option<i32>,
    ms: Option<f64>,
    error: Option<String>,
    /// Only where the side answered.
    by_duplicate: Option<bool>,
}

impl SideRow {
    fn of(side: &KnownItemSide) -> Self {
        let int = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
        Self {
            rank: side.rank.map(int),
            count: side.count.map(int),
            ms: side.ms,
            error: side.error.clone(),
            by_duplicate: side.count.map(|_| side.by_duplicate),
        }
    }
}

impl ItemRow {
    fn of(run: i64, item: &KnownItemRow) -> Self {
        Self {
            run,
            item_id: item.item_id.clone(),
            alpha: item.alpha,
            status: item.status.as_str(),
            primary: SideRow::of(&item.primary),
            shadow: SideRow::of(&item.shadow),
            overlap: item.metrics.map(|m| m.overlap),
            jaccard: item.metrics.map(|m| m.jaccard),
            rbo: item.metrics.map(|m| m.rbo),
            top1: item.metrics.map(|m| m.top1),
            primary_ndcg: item.ndcg.map(|(p, _)| p),
            shadow_ndcg: item.ndcg.map(|(_, s)| s),
            population: item.population.as_str(),
            mode: item.mode.as_str(),
        }
    }

    fn values(&self) -> [&(dyn tokio_postgres::types::ToSql + Sync); ITEM_WIDTH] {
        [
            &self.run,
            &self.item_id,
            &self.alpha,
            &self.status,
            &self.primary.rank,
            &self.primary.count,
            &self.primary.ms,
            &self.primary.error,
            &self.shadow.rank,
            &self.shadow.count,
            &self.shadow.ms,
            &self.shadow.error,
            &self.overlap,
            &self.jaccard,
            &self.rbo,
            &self.top1,
            &self.primary_ndcg,
            &self.shadow_ndcg,
            &self.population,
            &self.mode,
            &self.primary.by_duplicate,
            &self.shadow.by_duplicate,
        ]
    }
}

#[cfg(test)]
mod tests;

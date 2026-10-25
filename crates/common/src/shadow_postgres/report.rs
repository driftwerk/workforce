//! What shadow mode's report is worked out from (DW-676, PR 9c), read from
//! Postgres alone: a store's raw comparisons over a window, the day's one
//! known-item run that ran to its end, the reconciles' fixes, and the
//! weekly verdicts of `V033__shadow_verdicts.sql`.
//!
//! Percentiles are always taken over the raw rows of the window asked for:
//! a week's p99 is the p99 of the week's comparisons, never anything made
//! of the days' p99s, which cannot be combined.

use chrono::{DateTime, NaiveDate, Utc};
use eyre::{Context, Result};

use super::{block_on, bounded, bounded_query, ShadowStore, RECORD_LIMIT};

/// Days a weekly verdict is kept.
pub const VERDICTS_RETENTION_DAYS: u32 = 180;

/// A store's comparisons over a window, as the gates read them. Agreement
/// and the first latency percentiles are over the answered samples, both
/// sides on the same questions; the `_wt` ones over the answered and the
/// timed out, each timeout at its `shadow_ms` (L1's measure, design r2
/// section 8.1).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowStats {
    pub samples: i64,
    pub answered: i64,
    pub failed: i64,
    pub panicked: i64,
    pub timed_out: i64,
    pub probes: i64,
    pub mean_overlap: Option<f64>,
    pub mean_rbo: Option<f64>,
    pub mean_jaccard: Option<f64>,
    pub top1_rate: Option<f64>,
    pub empty_primary_only: i64,
    pub empty_shadow_only: i64,
    pub primary: Percentiles,
    pub shadow: Percentiles,
    pub primary_wt: Percentiles,
    pub shadow_wt: Percentiles,
    /// Where vellev's time went, from PR 9b's labels where the rows carry
    /// them (answered samples only).
    pub parts: Breakdown,
}

/// The shadow's time in parts, over answered samples whose labels hold
/// PR 9b's breakdown: its wait to be scheduled (`shadow_queue_ms`, outside
/// `shadow_ms`), the index call alone (`shadow_index_ms`), and `shadow_ms`
/// split by whether it ran alone (`concurrent` at most 1) or in a burst.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Breakdown {
    /// Answered samples carrying the labels; 0 before PR 9b.
    pub labelled: i64,
    pub queue_p50: Option<f64>,
    pub queue_p99: Option<f64>,
    pub index_p50: Option<f64>,
    pub index_p99: Option<f64>,
    pub alone: i64,
    pub alone_p50: Option<f64>,
    pub alone_p99: Option<f64>,
    pub burst: i64,
    pub burst_p50: Option<f64>,
    pub burst_p99: Option<f64>,
}

/// A numeric label of a comparison, NULL when absent or not a number.
fn label(name: &str) -> String {
    format!("(CASE WHEN jsonb_typeof(labels->'{name}') = 'number' THEN (labels->>'{name}')::float8 END)")
}

/// p50, p95 and p99 of one side's index-call time, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Percentiles {
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
}

/// A fix one reconcile made, with when that reconcile finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedFix {
    pub at: DateTime<Utc>,
    pub kind: String,
    pub memory_id: Option<String>,
}

/// The reconciles of a store over a window: how many ran to their end,
/// the fixes they named, and how many more they counted without naming.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileWindow {
    pub ok_runs: i64,
    pub other_runs: i64,
    pub fixes: Vec<RecordedFix>,
    pub fixes_left_out: i64,
}

/// A recorded weekly verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedVerdict {
    pub store: String,
    pub week_start: NaiveDate,
    pub week_end: NaiveDate,
    pub verdict: String,
    pub report: serde_json::Value,
    pub recorded_at: DateTime<Utc>,
}

/// Midnight UTC at the start of `day`.
pub fn day_start(day: NaiveDate) -> DateTime<Utc> {
    super::midnight(day)
}

impl ShadowStore {
    /// `store`'s comparisons from `from` (inclusive) to `to` (exclusive),
    /// worked out from the raw rows.
    pub async fn window_stats(
        &self,
        store: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<WindowStats> {
        let mut percentiles = Vec::new();
        for of in [
            "outcome = 'answered'",
            "outcome IN ('answered', 'timed_out')",
        ] {
            for side in ["primary", "shadow"] {
                for p in ["0.5", "0.95", "0.99"] {
                    percentiles.push(format!(
                        "percentile_cont({p}) WITHIN GROUP (ORDER BY {side}_ms) FILTER (WHERE {of})"
                    ));
                }
            }
        }
        let (queue, index, concurrent) = (
            label("shadow_queue_ms"),
            label("shadow_index_ms"),
            label("concurrent"),
        );
        let answered = "outcome = 'answered'";
        let pct = |p: &str, of: &str, filter: &str| {
            format!("percentile_cont({p}) WITHIN GROUP (ORDER BY {of}) FILTER (WHERE {answered} AND {filter})")
        };
        let alone = format!("{concurrent} <= 1");
        let burst = format!("{concurrent} > 1");
        let parts = [
            format!("count(*) FILTER (WHERE {answered} AND {queue} IS NOT NULL)"),
            pct("0.5", &queue, &format!("{queue} IS NOT NULL")),
            pct("0.99", &queue, &format!("{queue} IS NOT NULL")),
            pct("0.5", &index, &format!("{index} IS NOT NULL")),
            pct("0.99", &index, &format!("{index} IS NOT NULL")),
            format!("count(*) FILTER (WHERE {answered} AND {alone})"),
            pct("0.5", "shadow_ms", &alone),
            pct("0.99", "shadow_ms", &alone),
            format!("count(*) FILTER (WHERE {answered} AND {burst})"),
            pct("0.5", "shadow_ms", &burst),
            pct("0.99", "shadow_ms", &burst),
        ];
        percentiles.extend(parts);
        let sql = format!(
            "SELECT count(*),
                 count(*) FILTER (WHERE outcome = 'answered'),
                 count(*) FILTER (WHERE outcome = 'failed'),
                 count(*) FILTER (WHERE outcome = 'panicked'),
                 count(*) FILTER (WHERE outcome = 'timed_out'),
                 count(*) FILTER (WHERE probe),
                 avg(overlap) FILTER (WHERE outcome = 'answered'),
                 avg(rbo) FILTER (WHERE outcome = 'answered'),
                 avg(jaccard) FILTER (WHERE outcome = 'answered'),
                 (avg(CASE WHEN top1 THEN 1.0 ELSE 0.0 END)
                     FILTER (WHERE outcome = 'answered'))::float8,
                 count(*) FILTER (WHERE outcome = 'answered' AND empty_primary_only),
                 count(*) FILTER (WHERE outcome = 'answered' AND empty_shadow_only),
                 {}
             FROM shadow_comparisons
             WHERE store = $1 AND at >= $2 AND at < $3",
            percentiles.join(",\n                 ")
        );
        let rows = bounded_query(
            &self.pool,
            self.retention_limit,
            &sql,
            &[&store, &from, &to],
        )
        .await
        .context("reading a window of shadow comparisons")?;
        let r = rows
            .first()
            .ok_or_else(|| eyre::eyre!("an aggregate gave no row"))?;
        let p = |at: usize| Percentiles {
            p50: r.get(at),
            p95: r.get(at + 1),
            p99: r.get(at + 2),
        };
        Ok(WindowStats {
            samples: r.get(0),
            answered: r.get(1),
            failed: r.get(2),
            panicked: r.get(3),
            timed_out: r.get(4),
            probes: r.get(5),
            mean_overlap: r.get(6),
            mean_rbo: r.get(7),
            mean_jaccard: r.get(8),
            top1_rate: r.get(9),
            empty_primary_only: r.get(10),
            empty_shadow_only: r.get(11),
            primary: p(12),
            shadow: p(15),
            primary_wt: p(18),
            shadow_wt: p(21),
            parts: Breakdown {
                labelled: r.get(24),
                queue_p50: r.get(25),
                queue_p99: r.get(26),
                index_p50: r.get(27),
                index_p99: r.get(28),
                alone: r.get(29),
                alone_p50: r.get(30),
                alone_p99: r.get(31),
                burst: r.get(32),
                burst_p50: r.get(33),
                burst_p99: r.get(34),
            },
        })
    }

    /// The known-item run of `store` for `day` that counts: the one that
    /// ran to its end (`ok`). A day has at most one (the partial unique
    /// index); runs stopped part way, failed or abandoned never count.
    /// Returns the run's id and the serving alpha it was asked at.
    pub async fn counted_known_item_run(
        &self,
        store: &str,
        day: NaiveDate,
    ) -> Result<Option<(i64, f32)>> {
        let rows = bounded_query(
            &self.pool,
            RECORD_LIMIT,
            "SELECT id, alpha_serving FROM shadow_known_item_runs
             WHERE store = $1 AND run_day = $2 AND outcome = 'ok'
             ORDER BY id LIMIT 1",
            &[&store, &day],
        )
        .await
        .context("finding a day's known-item run")?;
        Ok(rows.first().map(|r| (r.get(0), r.get(1))))
    }

    /// Distinct items both stores answered, over `runs`, of one population,
    /// query mode and alpha: an item drawn on several nights counts once.
    pub async fn distinct_known_items(
        &self,
        runs: &[i64],
        population: &str,
        mode: &str,
        alpha: f32,
    ) -> Result<i64> {
        let rows = bounded_query(
            &self.pool,
            RECORD_LIMIT,
            "SELECT count(DISTINCT item_id) FROM shadow_known_items
             WHERE run_id = ANY($1) AND population = $2 AND mode = $3 AND alpha = $4
               AND status = 'asked' AND primary_error IS NULL AND shadow_error IS NULL",
            &[&runs, &population, &mode, &alpha],
        )
        .await
        .context("counting a week's distinct known items")?;
        Ok(rows.first().map(|r| r.get(0)).unwrap_or(0))
    }

    /// The reconciles of `store` that finished from `from` to `to`.
    pub async fn reconcile_window(
        &self,
        store: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<ReconcileWindow> {
        let rows = bounded_query(
            &self.pool,
            RECORD_LIMIT,
            "SELECT outcome, finished_at, fixed, fixed_left_out FROM shadow_reconciles
             WHERE store = $1 AND finished_at >= $2 AND finished_at < $3
             ORDER BY finished_at",
            &[&store, &from, &to],
        )
        .await
        .context("reading a window of reconciles")?;
        let mut window = ReconcileWindow::default();
        for row in rows {
            let outcome: String = row.get(0);
            if outcome == "ok" {
                window.ok_runs += 1;
            } else {
                window.other_runs += 1;
            }
            let at: DateTime<Utc> = row.get(1);
            let fixed: serde_json::Value = row.get(2);
            for fix in fixed.as_array().into_iter().flatten() {
                window.fixes.push(RecordedFix {
                    at,
                    kind: fix["kind"].as_str().unwrap_or("unknown").to_string(),
                    memory_id: fix["memory_id"].as_str().map(str::to_string),
                });
            }
            window.fixes_left_out += i64::from(row.get::<_, i32>(3));
        }
        Ok(window)
    }

    /// The memory mirror's backlog, read on this store's own pool: the
    /// pending list is in the same schema, so reading it needs no pool of
    /// its own.
    pub async fn mirror_backlog(&self) -> Result<super::Backlog> {
        let rows = bounded_query(
            &self.pool,
            super::MIRROR_NOTE_LIMIT,
            "SELECT count(*), min(noted_at) FROM memory_mirror_pending",
            &[],
        )
        .await
        .context("reading the memory mirror's backlog")?;
        let row = rows
            .first()
            .ok_or_else(|| eyre::eyre!("count gave no row"))?;
        Ok(super::Backlog {
            pending: row.get(0),
            oldest: row.get(1),
        })
    }

    /// Record `verdict` for `store`'s week from `week_start`, replacing one
    /// recorded before.
    pub async fn record_verdict(
        &self,
        store: &str,
        week_start: NaiveDate,
        week_end: NaiveDate,
        verdict: &str,
        report: &serde_json::Value,
    ) -> Result<()> {
        bounded(
            &self.pool,
            RECORD_LIMIT,
            "INSERT INTO shadow_verdicts (store, week_start, week_end, verdict, report)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (store, week_start) DO UPDATE SET
                 week_end = EXCLUDED.week_end, verdict = EXCLUDED.verdict,
                 report = EXCLUDED.report, recorded_at = now()",
            &[&store, &week_start, &week_end, &verdict, report],
        )
        .await
        .context("recording a shadow verdict")?;
        Ok(())
    }

    /// The verdicts recorded, newest week first: of `store`, or of every
    /// store.
    pub async fn verdicts(&self, store: Option<&str>, limit: i64) -> Result<Vec<RecordedVerdict>> {
        let rows = bounded_query(
            &self.pool,
            RECORD_LIMIT,
            "SELECT store, week_start, week_end, verdict, report, recorded_at
             FROM shadow_verdicts WHERE $1::text IS NULL OR store = $1
             ORDER BY week_start DESC, store LIMIT $2",
            &[&store, &limit],
        )
        .await
        .context("reading shadow verdicts")?;
        Ok(rows
            .iter()
            .map(|r| RecordedVerdict {
                store: r.get(0),
                week_start: r.get(1),
                week_end: r.get(2),
                verdict: r.get(3),
                report: r.get(4),
                recorded_at: r.get(5),
            })
            .collect())
    }

    /// Each store and UTC day that holds comparisons before `before`.
    pub async fn compared_days(&self, before: DateTime<Utc>) -> Result<Vec<(String, NaiveDate)>> {
        let rows = bounded_query(
            &self.pool,
            self.retention_limit,
            "SELECT DISTINCT store, (at AT TIME ZONE 'UTC')::date FROM shadow_comparisons
             WHERE at < $1 ORDER BY 1, 2",
            &[&before],
        )
        .await
        .context("finding the days compared")?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// The verdicts' retention pass: weeks that ended before `before`'s
    /// day deleted. Returns how many.
    pub fn prune_verdicts(&self, before: DateTime<Utc>) -> Result<usize> {
        let cutoff = before.date_naive();
        block_on(async {
            Ok(bounded(
                &self.pool,
                self.retention_limit,
                "DELETE FROM shadow_verdicts WHERE week_end < $1",
                &[&cutoff],
            )
            .await? as usize)
        })
    }
}

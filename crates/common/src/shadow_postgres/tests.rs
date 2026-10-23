use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, Utc};

use super::*;
use crate::shadow::{Comparison, Observed, Outcome, RankMetrics, Ranked, ShadowSink};
use crate::telemetry_postgres::PostgresTelemetryStore;

/// Runs `test` against a fresh database, unless Postgres tests are skipped.
async fn with_store<F, Fut>(test: F)
where
    F: FnOnce(Arc<ShadowStore>, PostgresPool) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    workforce_postgres::test_support::with_test_pool(
        PostgresTelemetryStore::build_pool,
        |pool| async move {
            let store = ShadowStore::with_pool(pool.clone()).await.unwrap();
            test(store, pool).await
        },
    )
    .await;
}

fn ranked(ids: &[&str]) -> Ranked {
    Ranked {
        ids: ids.iter().map(|s| s.to_string()).collect(),
        top_score: Some(0.5),
        embed: Some(Duration::from_millis(3)),
    }
}

fn answered(store: &'static str, primary: &[&str], shadow: &[&str]) -> Comparison {
    let p = ranked(primary);
    let s = ranked(shadow);
    let metrics = RankMetrics::of(&p.ids, &s.ids, 10, 0.9);
    Comparison {
        store,
        primary: Observed {
            ranked: p,
            latency: Duration::from_millis(12),
            query_hash: Some("h1".into()),
            labels: vec![("same_model", "true".into())],
        },
        shadow_latency: Duration::from_millis(20),
        outcome: Outcome::Answered { ranked: s, metrics },
        probe: false,
    }
}

/// An unanswered row: outcome, error, probe, shadow count, overlap, k.
type Unanswered = (
    String,
    Option<String>,
    bool,
    Option<i32>,
    Option<f64>,
    Option<i32>,
);

fn unanswered(outcome: Outcome) -> Comparison {
    Comparison {
        outcome,
        probe: true,
        ..answered("memories", &["a"], &["a"])
    }
}

async fn count(pool: &PostgresPool, sql: &str) -> i64 {
    pool.get()
        .await
        .unwrap()
        .query_one(sql, &[])
        .await
        .unwrap()
        .get(0)
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let began = Instant::now();
    while !done() {
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "waited for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---- The sink ----------------------------------------------------------------

/// What is recorded is written, each outcome as it went, and every sample
/// is accounted for.
#[tokio::test(flavor = "multi_thread")]
async fn the_sink_writes_each_comparison_as_it_went() {
    with_store(|store, pool| async move {
        let sink = store.sink(64, Duration::from_secs(10));
        sink.record(answered("artifacts", &["a", "b", "c"], &["a", "c", "b"]));
        sink.record(unanswered(Outcome::Failed("refused: no".into())));
        sink.record(unanswered(Outcome::TimedOut));
        sink.record(unanswered(Outcome::Panicked));
        until("four written", || sink.counts().written == 4).await;
        assert_eq!(
            sink.counts(),
            SinkCounts {
                recorded: 4,
                dropped: 0,
                written: 4,
                failed: 0
            }
        );

        let conn = pool.get().await.unwrap();
        let row = conn
            .query_one(
                "SELECT store, outcome, error, probe, query_hash, labels->>'same_model',
                        primary_ms, primary_embed_ms, primary_count, primary_top_score,
                        shadow_ms, shadow_embed_ms, shadow_count, k, overlap, jaccard, rbo,
                        top1, empty_primary_only, empty_shadow_only
                 FROM shadow_comparisons WHERE outcome = 'answered'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "artifacts");
        assert_eq!(row.get::<_, Option<String>>(2), None);
        assert!(!row.get::<_, bool>(3));
        assert_eq!(row.get::<_, Option<String>>(4).as_deref(), Some("h1"));
        assert_eq!(row.get::<_, Option<String>>(5).as_deref(), Some("true"));
        assert_eq!(row.get::<_, f64>(6), 12.0);
        assert_eq!(row.get::<_, Option<f64>>(7), Some(3.0));
        assert_eq!(row.get::<_, i32>(8), 3);
        assert_eq!(row.get::<_, Option<f32>>(9), Some(0.5));
        assert_eq!(row.get::<_, f64>(10), 20.0);
        assert_eq!(row.get::<_, Option<f64>>(11), Some(3.0));
        assert_eq!(row.get::<_, Option<i32>>(12), Some(3));
        assert_eq!(row.get::<_, Option<i32>>(13), Some(10));
        assert_eq!(row.get::<_, Option<f64>>(14), Some(1.0));
        assert_eq!(row.get::<_, Option<f64>>(15), Some(1.0));
        let rbo = row.get::<_, Option<f64>>(16).unwrap();
        assert!((rbo - 0.955).abs() < 1e-9, "{rbo}");
        assert_eq!(row.get::<_, Option<bool>>(17), Some(true));
        assert_eq!(row.get::<_, Option<bool>>(18), Some(false));
        assert_eq!(row.get::<_, Option<bool>>(19), Some(false));

        let rows = conn
            .query(
                "SELECT outcome, error, probe, shadow_count, overlap, k
                 FROM shadow_comparisons WHERE outcome <> 'answered' ORDER BY outcome",
                &[],
            )
            .await
            .unwrap();
        let got: Vec<Unanswered> = rows
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5)))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "failed".into(),
                    Some("refused: no".into()),
                    true,
                    None,
                    None,
                    None
                ),
                ("panicked".into(), None, true, None, None, None),
                ("timed_out".into(), None, true, None, None, None),
            ]
        );
    })
    .await;
}

/// Postgres hung: every `record` returns at once, what does not fit is
/// dropped and counted, nothing waits in a queue past the sink's capacity,
/// and once Postgres answers again everything kept is written.
#[tokio::test(flavor = "multi_thread")]
async fn the_sink_never_blocks_its_caller_when_postgres_hangs() {
    with_store(|store, pool| async move {
        let capacity = 16;
        let sink = store.sink(capacity, Duration::from_secs(60));
        // A transaction holding the table: every insert waits on it.
        let holder = pool.get().await.unwrap();
        holder
            .batch_execute("BEGIN; LOCK TABLE shadow_comparisons IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();

        let n = 500;
        let began = Instant::now();
        for _ in 0..n {
            let one = Instant::now();
            sink.record(answered("memories", &["a"], &["a"]));
            assert!(
                one.elapsed() < Duration::from_millis(50),
                "record took {:?}",
                one.elapsed()
            );
        }
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "{:?}",
            began.elapsed()
        );
        // The writer holds at most one batch, the channel at most its
        // capacity: the rest were dropped.
        let counts = sink.counts();
        assert_eq!(counts.recorded, n);
        assert!(
            counts.dropped >= n - capacity as u64 - SINK_BATCH as u64,
            "{counts:?}"
        );
        assert_eq!(counts.written, 0, "written through a held lock");

        holder.batch_execute("COMMIT").await.unwrap();
        until("all kept written", || {
            let c = sink.counts();
            c.written + c.dropped + c.failed == n
        })
        .await;
        let counts = sink.counts();
        assert_eq!(counts.failed, 0, "{counts:?}");
        assert_eq!(
            count(&pool, "SELECT count(*) FROM shadow_comparisons").await as u64,
            counts.written
        );
    })
    .await;
}

/// An insert that does not finish in time is given up and counted as
/// failed, and the writer goes on: later samples are written.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_that_times_out_is_counted_and_the_writer_goes_on() {
    with_store(|store, pool| async move {
        let sink = store.sink(64, Duration::from_millis(200));
        let holder = pool.get().await.unwrap();
        holder
            .batch_execute("BEGIN; LOCK TABLE shadow_comparisons IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        sink.record(answered("memories", &["a"], &["a"]));
        until("the batch given up", || sink.counts().failed == 1).await;
        holder.batch_execute("COMMIT").await.unwrap();

        sink.record(answered("memories", &["a"], &["b"]));
        until("the next written", || sink.counts().written == 1).await;
        assert_eq!(
            sink.counts(),
            SinkCounts {
                recorded: 2,
                dropped: 0,
                written: 1,
                failed: 1
            }
        );
    })
    .await;
}

// ---- Retention -----------------------------------------------------------------

/// A comparison at `at`, written directly.
#[allow(clippy::too_many_arguments)]
async fn at(
    pool: &PostgresPool,
    store: &str,
    at: DateTime<Utc>,
    outcome: &str,
    primary_ms: f64,
    shadow_ms: f64,
    overlap: Option<f64>,
    top1: Option<bool>,
) {
    pool.get()
        .await
        .unwrap()
        .execute(
            "INSERT INTO shadow_comparisons
                 (store, at, outcome, probe, primary_ms, primary_count, shadow_ms,
                  overlap, jaccard, rbo, top1, empty_primary_only, empty_shadow_only)
             VALUES ($1, $2, $3, false, $4, 1, $5, $6, $6, $6, $7, false, $8)",
            &[
                &store,
                &at,
                &outcome,
                &primary_ms,
                &shadow_ms,
                &overlap,
                &top1,
                &(outcome == "answered" && overlap == Some(0.0)),
            ],
        )
        .await
        .unwrap();
}

fn midnight(day: NaiveDate) -> DateTime<Utc> {
    day.and_hms_opt(0, 0, 0).unwrap().and_utc()
}

async fn daily(pool: &PostgresPool, store: &str, day: NaiveDate) -> Option<Vec<Option<f64>>> {
    let row = pool
        .get()
        .await
        .unwrap()
        .query_opt(
            "SELECT samples::float8, answered::float8, failed::float8, timed_out::float8,
                    mean_overlap, top1_rate, empty_shadow_only::float8,
                    primary_p50_ms, primary_p99_ms, shadow_p50_ms, shadow_p99_ms
             FROM shadow_daily WHERE store = $1 AND day = $2",
            &[&store, &day],
        )
        .await
        .unwrap()?;
    Some((0..11).map(|i| row.get::<_, Option<f64>>(i)).collect())
}

/// A day is rolled up from its comparisons: counts by outcome, agreement
/// and latency over the answered ones, both sides' percentiles on the same
/// questions. Each store's day is its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_day_is_rolled_up_from_its_comparisons() {
    with_store(|store, pool| async move {
        let today = Utc::now().date_naive();
        let day = today - chrono::Days::new(2);
        let noon = midnight(day) + chrono::Duration::hours(12);
        // Answered: primary 10, 20, 30, 40 ms; shadow 15, 25, 35, 45 ms;
        // overlap 1, 1, 0.5, 0; top-1 yes, yes, yes, no.
        for (p, s, o, t) in [
            (10.0, 15.0, 1.0, true),
            (20.0, 25.0, 1.0, true),
            (30.0, 35.0, 0.5, true),
            (40.0, 45.0, 0.0, false),
        ] {
            at(&pool, "memories", noon, "answered", p, s, Some(o), Some(t)).await;
        }
        // Not answered: counted, never in agreement or the answered-only
        // latency. The timeout is in the gate's latency, at its shadow_ms.
        at(&pool, "memories", noon, "failed", 500.0, 1.0, None, None).await;
        at(
            &pool,
            "memories",
            noon,
            "timed_out",
            900.0,
            2000.0,
            None,
            None,
        )
        .await;
        at(
            &pool,
            "artifacts",
            noon,
            "answered",
            7.0,
            9.0,
            Some(1.0),
            Some(true),
        )
        .await;
        // Today is not over, so not rolled up.
        at(
            &pool,
            "memories",
            Utc::now(),
            "answered",
            1.0,
            1.0,
            Some(1.0),
            Some(true),
        )
        .await;

        let before = Utc::now() - chrono::Duration::days(i64::from(COMPARISONS_RETENTION_DAYS));
        store.roll_up_and_prune_comparisons(before).unwrap();

        // percentile_cont over 10, 20, 30, 40: p50 25, p99 39.7.
        let want = |v: &[f64]| v.iter().map(|x| Some(*x)).collect::<Vec<_>>();
        let got = daily(&pool, "memories", day).await.expect("rolled up");
        let close = |a: &[Option<f64>], b: &[Option<f64>]| {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(x, y)| (x.unwrap() - y.unwrap()).abs() < 1e-9)
        };
        let memories = want(&[6.0, 4.0, 1.0, 1.0, 0.625, 0.75, 1.0, 25.0, 39.7, 30.0, 44.7]);
        assert!(close(&got, &memories), "{got:?}");
        let artifacts = daily(&pool, "artifacts", day).await.expect("rolled up");
        assert!(
            close(
                &artifacts,
                &want(&[1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 7.0, 7.0, 9.0, 9.0])
            ),
            "{artifacts:?}"
        );
        assert!(
            daily(&pool, "memories", today).await.is_none(),
            "today is not over"
        );

        // The gate's (L1) percentiles count the timeout: primary over 10,
        // 20, 30, 40, 900 (p50 30, p99 40 + 0.96 * 860 = 865.6); shadow
        // over 15, 25, 35, 45, 2000 (p50 35, p99 45 + 0.96 * 1955 = 1921.8).
        let row = pool
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT primary_p50_ms_with_timeouts, primary_p99_ms_with_timeouts,
                        shadow_p50_ms_with_timeouts, shadow_p99_ms_with_timeouts
                 FROM shadow_daily WHERE store = 'memories' AND day = $1",
                &[&day],
            )
            .await
            .unwrap();
        let gate: Vec<f64> = (0..4).map(|i| row.get::<_, f64>(i)).collect();
        for (got, want) in gate.iter().zip([30.0, 865.6, 35.0, 1921.8]) {
            assert!((got - want).abs() < 1e-9, "{gate:?}");
        }
    })
    .await;
}

/// Comparisons go a whole UTC day at a time: from before the cutoff's day,
/// once rolled up; the cutoff's own day and later are kept.
#[tokio::test(flavor = "multi_thread")]
async fn comparisons_go_whole_days_past_retention_once_rolled_up() {
    with_store(|store, pool| async move {
        let now = Utc::now();
        let before = now - chrono::Duration::days(i64::from(COMPARISONS_RETENTION_DAYS));
        let cutoff_day = before.date_naive();
        let gone_day = cutoff_day - chrono::Days::new(1);
        // The last instant of the day before the cutoff's, and the first
        // of the cutoff's own day.
        let last_gone = midnight(cutoff_day) - chrono::Duration::microseconds(1);
        at(
            &pool,
            "memories",
            last_gone,
            "answered",
            1.0,
            1.0,
            Some(1.0),
            Some(true),
        )
        .await;
        at(
            &pool,
            "memories",
            midnight(cutoff_day),
            "answered",
            1.0,
            1.0,
            Some(1.0),
            Some(true),
        )
        .await;
        at(
            &pool,
            "memories",
            now,
            "answered",
            1.0,
            1.0,
            Some(1.0),
            Some(true),
        )
        .await;

        let removed = store.roll_up_and_prune_comparisons(before).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(
            count(&pool, "SELECT count(*) FROM shadow_comparisons").await,
            2
        );
        let rolled = daily(&pool, "memories", gone_day)
            .await
            .expect("rolled up first");
        assert_eq!(rolled[0], Some(1.0), "its one sample");
    })
    .await;
}

/// A day past retention already rolled up is never rolled up again: a
/// prune stopped part way through it cannot overwrite its total with
/// what is left.
#[tokio::test(flavor = "multi_thread")]
async fn a_day_past_retention_keeps_its_total() {
    with_store(|store, pool| async move {
        let before = Utc::now() - chrono::Duration::days(i64::from(COMPARISONS_RETENTION_DAYS));
        let old = before.date_naive() - chrono::Days::new(3);
        let noon = midnight(old) + chrono::Duration::hours(12);
        for _ in 0..3 {
            at(&pool, "memories", noon, "answered", 1.0, 1.0, Some(1.0), Some(true)).await;
        }
        // Rolled up while it was still inside the window.
        let earlier = midnight(old) - chrono::Duration::days(1);
        store.roll_up_and_prune_comparisons(earlier).unwrap();
        assert_eq!(daily(&pool, "memories", old).await.unwrap()[0], Some(3.0));
        // Then part of the day is gone, as after a pass that stopped.
        pool.get()
            .await
            .unwrap()
            .execute(
                "DELETE FROM shadow_comparisons WHERE id = (SELECT min(id) FROM shadow_comparisons)",
                &[],
            )
            .await
            .unwrap();
        store.roll_up_and_prune_comparisons(before).unwrap();
        assert_eq!(
            daily(&pool, "memories", old).await.unwrap()[0],
            Some(3.0),
            "overwritten with what was left"
        );
    })
    .await;
}

/// Days of the daily rollup go past their own retention, a whole day at
/// a time.
#[tokio::test(flavor = "multi_thread")]
async fn daily_rows_go_past_their_retention() {
    with_store(|store, pool| async move {
        let before = Utc::now() - chrono::Duration::days(i64::from(DAILY_RETENTION_DAYS));
        let cutoff_day = before.date_naive();
        for day in [cutoff_day - chrono::Days::new(1), cutoff_day] {
            pool.get()
                .await
                .unwrap()
                .execute(
                    "INSERT INTO shadow_daily (store, day, samples, answered, failed, panicked,
                         timed_out, probes, empty_primary_only, empty_shadow_only)
                     VALUES ('memories', $1, 1, 1, 0, 0, 0, 0, 0, 0)",
                    &[&day],
                )
                .await
                .unwrap();
        }
        assert_eq!(store.prune_daily(before).unwrap(), 1);
        assert!(daily(&pool, "memories", cutoff_day).await.is_some());
        assert!(daily(&pool, "memories", cutoff_day - chrono::Days::new(1))
            .await
            .is_none());
    })
    .await;
}

/// A batch Postgres gives up is not written later: the statement is given
/// up by Postgres itself, not only by the writer, here while it runs (a
/// trigger that sleeps a second, with a limit of 200 ms).
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_given_up_is_never_written_later() {
    with_store(|store, pool| async move {
        pool.get()
            .await
            .unwrap()
            .batch_execute(
                "CREATE FUNCTION slow_shadow_insert() RETURNS trigger AS $$
                 BEGIN PERFORM pg_sleep(1); RETURN NEW; END; $$ LANGUAGE plpgsql;
                 CREATE TRIGGER slow BEFORE INSERT ON shadow_comparisons
                     FOR EACH ROW EXECUTE FUNCTION slow_shadow_insert();",
            )
            .await
            .unwrap();
        let sink = store.sink(64, Duration::from_millis(200));
        sink.record(answered("memories", &["a"], &["a"]));
        until("the batch given up", || sink.counts().failed == 1).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            count(&pool, "SELECT count(*) FROM shadow_comparisons").await,
            0,
            "a batch counted as failed was written"
        );
    })
    .await;
}

/// A shadow table that hangs costs retention one pass, given up within its
/// limit, rather than the hub's retention task for good.
#[tokio::test(flavor = "multi_thread")]
async fn retention_is_given_up_when_a_shadow_table_hangs() {
    workforce_postgres::test_support::with_test_pool(
        PostgresTelemetryStore::build_pool,
        |pool| async move {
            let store = ShadowStore::with_limit(pool.clone(), Duration::from_millis(200))
                .await
                .unwrap();
            let holder = pool.get().await.unwrap();
            holder
                .batch_execute(
                    "BEGIN; LOCK TABLE shadow_comparisons, shadow_daily IN ACCESS EXCLUSIVE MODE",
                )
                .await
                .unwrap();
            let before = Utc::now() - chrono::Duration::days(14);
            let began = Instant::now();
            assert!(store.roll_up_and_prune_comparisons(before).is_err());
            assert!(
                began.elapsed() < Duration::from_secs(3),
                "{:?}",
                began.elapsed()
            );
            let began = Instant::now();
            assert!(store.prune_daily(before).is_err());
            assert!(
                began.elapsed() < Duration::from_secs(3),
                "{:?}",
                began.elapsed()
            );
            holder.batch_execute("COMMIT").await.unwrap();
            assert!(store.roll_up_and_prune_comparisons(before).is_ok());
        },
    )
    .await;
}

/// A day is a UTC day, whatever the time zone of the session that rolls it
/// up: here 14 hours ahead, where 23:30 UTC is already the next day.
#[tokio::test(flavor = "multi_thread")]
async fn a_day_is_a_utc_day_whatever_the_session_time_zone() {
    workforce_postgres::test_support::with_test_pool(
        |config| {
            PostgresPool::from_config_with_session_init(
                config,
                &[
                    "SET search_path TO telemetry, public",
                    "SET TIME ZONE 'Pacific/Kiritimati'",
                ],
            )
        },
        |pool| async move {
            let store = ShadowStore::with_pool(pool.clone()).await.unwrap();
            let day = Utc::now().date_naive() - chrono::Days::new(3);
            let late = midnight(day) + chrono::Duration::minutes(23 * 60 + 30);
            at(
                &pool,
                "memories",
                late,
                "answered",
                1.0,
                1.0,
                Some(1.0),
                Some(true),
            )
            .await;
            let before = Utc::now() - chrono::Duration::days(i64::from(COMPARISONS_RETENTION_DAYS));
            store.roll_up_and_prune_comparisons(before).unwrap();
            assert!(
                daily(&pool, "memories", day).await.is_some(),
                "not on its UTC day"
            );
            assert!(daily(&pool, "memories", day + chrono::Days::new(1))
                .await
                .is_none());
        },
    )
    .await;
}

/// With its writer gone, a sample is dropped and counted, as when there is
/// no room: never waited for, never lost uncounted.
#[tokio::test(flavor = "multi_thread")]
async fn a_sample_is_dropped_and_counted_once_the_writer_is_gone() {
    with_store(|store, _pool| async move {
        let sink = store.sink(64, Duration::from_secs(10));
        sink.writer.abort();
        until("the writer gone", || sink.tx.is_closed()).await;
        sink.record(answered("memories", &["a"], &["a"]));
        assert_eq!(
            sink.counts(),
            SinkCounts {
                recorded: 1,
                dropped: 1,
                written: 0,
                failed: 0
            }
        );
    })
    .await;
}

/// The applier's cursor (DW-676): paged one at a time from the start, the
/// due notes come back each once, in the list's order: never failed
/// (`next_try_at` NULL) first by id, then by when due, ties by id. Notes
/// not yet due are never given.
#[tokio::test(flavor = "multi_thread")]
async fn the_due_notes_are_paged_by_a_cursor_each_once_in_order() {
    with_store(|_, pool| async move {
        let notes = MirrorNotes::with_pool(pool.clone()).await.unwrap();
        let ids = ["a", "b", "c", "d", "e", "f", "g", "h"];
        notes
            .note(&ids.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .await
            .unwrap();
        let now: DateTime<Utc> = "2026-10-21T12:00:00Z".parse().unwrap();
        let early: DateTime<Utc> = "2026-10-21T11:00:00Z".parse().unwrap();
        let later: DateTime<Utc> = "2026-10-21T11:30:00Z".parse().unwrap();
        let future: DateTime<Utc> = "2026-10-21T13:00:00Z".parse().unwrap();
        // b, f: NULL. a, e, h tie at `early`; c at `later`; d, g not yet due.
        for (id, at) in [
            ("a", early),
            ("e", early),
            ("h", early),
            ("c", later),
            ("d", future),
            ("g", future),
        ] {
            pool.get()
                .await
                .unwrap()
                .execute(
                    "UPDATE memory_mirror_pending SET tries = 1, next_try_at = $2 WHERE memory_id = $1",
                    &[&id, &at],
                )
                .await
                .unwrap();
        }
        let mut seen = Vec::new();
        let mut after: Option<DueNote> = None;
        for _ in 0..20 {
            let page = notes.due(now, after.as_ref(), 1).await.unwrap();
            assert!(page.len() <= 1, "{page:?}");
            let Some(due) = page.into_iter().next() else {
                break;
            };
            seen.push(due.id.clone());
            after = Some(due);
        }
        assert_eq!(seen, ["b", "f", "a", "e", "h", "c"]);
    })
    .await;
}

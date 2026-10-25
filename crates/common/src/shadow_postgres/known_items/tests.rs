use std::sync::Arc;

use chrono::{Duration as Days, NaiveDate, Utc};

use super::*;
use crate::postgres::PostgresPool;
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

fn day(d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
}

fn claim(store: &str, run_day: NaiveDate) -> KnownItemClaim {
    KnownItemClaim {
        store: store.to_string(),
        run_day,
        base_seed: 7,
        run_seed: u64::MAX - 1,
        derivation: 1,
        k: 10,
        alpha_serving: 0.75,
        items_wanted: 1000,
    }
}

const STALE: Duration = KNOWN_ITEM_STALE;

async fn outcome_of(pool: &PostgresPool, run: i64) -> String {
    pool.get()
        .await
        .unwrap()
        .query_one(
            "SELECT outcome FROM shadow_known_item_runs WHERE id = $1",
            &[&run],
        )
        .await
        .unwrap()
        .get(0)
}

/// A store's day is claimed once: while its run is running, and once it
/// ran to its end. Another store, or another day, is claimed apart. A run
/// that ended early frees its day, to be run again.
#[tokio::test]
async fn a_day_is_claimed_once_while_its_run_runs_and_once_it_ran_to_its_end() {
    with_store(|store, pool| async move {
        let first = store
            .claim_known_item_run(&claim("memories", day(20)), STALE)
            .await
            .unwrap()
            .expect("an unclaimed day is claimed");
        assert_eq!(
            store
                .claim_known_item_run(&claim("memories", day(20)), STALE)
                .await
                .unwrap(),
            None,
            "a day claimed twice while its run runs"
        );
        assert!(store
            .claim_known_item_run(&claim("artifacts", day(20)), STALE)
            .await
            .unwrap()
            .is_some());
        let next_day = store
            .claim_known_item_run(&claim("memories", day(21)), STALE)
            .await
            .unwrap()
            .expect("another day is claimed apart");

        assert!(store
            .finish_known_item_run(first, KnownItemOutcome::Ok, None)
            .await
            .unwrap());
        assert_eq!(outcome_of(&pool, first).await, "ok");
        assert_eq!(
            store
                .claim_known_item_run(&claim("memories", day(20)), STALE)
                .await
                .unwrap(),
            None,
            "a day run to its end claimed again"
        );

        assert!(store
            .finish_known_item_run(next_day, KnownItemOutcome::Incomplete, Some("paused"))
            .await
            .unwrap());
        assert!(
            store
                .claim_known_item_run(&claim("memories", day(21)), STALE)
                .await
                .unwrap()
                .is_some(),
            "a day whose run stopped part way is not run again"
        );
    })
    .await;
}

/// A day is started at most [`KNOWN_ITEM_ATTEMPTS`] times, whatever came
/// of each.
#[tokio::test]
async fn a_day_is_tried_a_bounded_number_of_times() {
    with_store(|store, _pool| async move {
        for attempt in 0..KNOWN_ITEM_ATTEMPTS {
            let run = store
                .claim_known_item_run(&claim("memories", day(20)), STALE)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("attempt {attempt} refused"));
            store
                .finish_known_item_run(run, KnownItemOutcome::Incomplete, None)
                .await
                .unwrap();
        }
        assert_eq!(
            store
                .claim_known_item_run(&claim("memories", day(20)), STALE)
                .await
                .unwrap(),
            None,
            "a day tried past its attempts"
        );
    })
    .await;
}

/// A run found still running long after it started is taken for
/// abandoned, its hub gone, and its day can be claimed again; a run
/// started lately is left running. An abandoned run, should its hub come
/// back, cannot be finished as anything else.
#[tokio::test]
async fn a_run_left_running_is_abandoned_and_its_day_freed() {
    with_store(|store, pool| async move {
        let old = store
            .claim_known_item_run(&claim("memories", day(20)), STALE)
            .await
            .unwrap()
            .unwrap();
        let recent = store
            .claim_known_item_run(&claim("memories", day(21)), STALE)
            .await
            .unwrap()
            .unwrap();
        pool.get()
            .await
            .unwrap()
            .execute(
                "UPDATE shadow_known_item_runs SET started_at = now() - interval '7 hours' WHERE id = $1",
                &[&old],
            )
            .await
            .unwrap();
        let again = store
            .claim_known_item_run(&claim("memories", day(20)), STALE)
            .await
            .unwrap()
            .expect("an abandoned run's day is claimed again");
        assert_ne!(again, old);
        assert_eq!(outcome_of(&pool, old).await, "abandoned");
        assert_eq!(outcome_of(&pool, recent).await, "running");
        assert!(
            !store
                .finish_known_item_run(old, KnownItemOutcome::Ok, None)
                .await
                .unwrap(),
            "an abandoned run finished"
        );
        assert_eq!(outcome_of(&pool, old).await, "abandoned");
    })
    .await;
}

/// A run's claim keeps what it is reproduced from: the day, both seeds (a
/// u64 seed past i64's range included), the derivation, k and alpha.
#[tokio::test]
async fn a_claim_keeps_what_its_run_is_made_from() {
    with_store(|store, pool| async move {
        let run = store
            .claim_known_item_run(&claim("memories", day(20)), STALE)
            .await
            .unwrap()
            .unwrap();
        store
            .record_known_items_drawn(
                run,
                &KnownItemsDrawn {
                    population: 13_914,
                    excluded: BTreeMap::from([("deleted", 12), ("too_short", 300)]),
                    drawn: 1000,
                    drawn_unscoped: 250,
                },
            )
            .await
            .unwrap();
        let row = pool
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT run_day, base_seed, run_seed, derivation, k, alpha_serving, items_wanted,
                        population, excluded, drawn, outcome, drawn_unscoped
                 FROM shadow_known_item_runs WHERE id = $1",
                &[&run],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, NaiveDate>(0), day(20));
        assert_eq!(row.get::<_, i64>(1), 7);
        assert_eq!(row.get::<_, String>(2), format!("{:016x}", u64::MAX - 1));
        assert_eq!(
            (
                row.get::<_, i32>(3),
                row.get::<_, i32>(4),
                row.get::<_, f32>(5),
                row.get::<_, i32>(6)
            ),
            (1, 10, 0.75, 1000)
        );
        assert_eq!(row.get::<_, Option<i32>>(7), Some(13_914));
        assert_eq!(
            row.get::<_, serde_json::Value>(8),
            serde_json::json!({"deleted": 12, "too_short": 300})
        );
        assert_eq!(row.get::<_, Option<i32>>(9), Some(1000));
        assert_eq!(row.get::<_, String>(10), "running");
        assert_eq!(row.get::<_, Option<i32>>(11), Some(250));
    })
    .await;
}

fn side(rank: Option<usize>) -> KnownItemSide {
    KnownItemSide {
        rank,
        count: Some(10),
        ms: Some(4.0),
        error: None,
        by_duplicate: false,
    }
}

fn failed(error: &str) -> KnownItemSide {
    KnownItemSide {
        error: Some(error.to_string()),
        ms: Some(2000.0),
        ..KnownItemSide::default()
    }
}

fn item(
    memory: &str,
    alpha: f32,
    status: KnownItemStatus,
    primary: KnownItemSide,
    shadow: KnownItemSide,
    agreement: Option<f64>,
) -> KnownItemRow {
    KnownItemRow {
        item_id: memory.to_string(),
        alpha,
        status,
        primary,
        shadow,
        metrics: agreement.map(|a| RankMetrics {
            k: 10,
            overlap: a,
            jaccard: a,
            rbo: a,
            top1: a == 1.0,
            empty_primary_only: false,
            empty_shadow_only: false,
        }),
        ndcg: None,
        population: Population::Scoped,
        mode: QueryMode::Window,
    }
}

/// A run's items summed by hand: the rates, reciprocal ranks and pairs are
/// over the items both sides answered, errors counted apart, never as
/// misses, and items never asked counted by why.
#[tokio::test]
async fn a_runs_items_sum_to_what_a_verdict_needs() {
    with_store(|store, _pool| async move {
        let run = store
            .claim_known_item_run(&claim("memories", day(20)), STALE)
            .await
            .unwrap()
            .unwrap();
        use KnownItemStatus::*;
        let none = KnownItemSide::default;
        let items = vec![
            item("m1", 1.0, Asked, side(Some(1)), side(Some(1)), Some(1.0)),
            item("m2", 1.0, Asked, side(Some(1)), side(Some(2)), Some(0.8)),
            item("m3", 1.0, Asked, side(Some(1)), side(None), Some(0.6)),
            item("m4", 1.0, Asked, side(Some(3)), side(Some(1)), Some(0.4)),
            item("m5", 1.0, Asked, side(None), side(None), Some(0.2)),
            item("m6", 1.0, Asked, side(Some(1)), failed("timed out"), None),
            item("m7", 1.0, EmbedFailed, none(), none(), None),
            item("m8", 1.0, NoPermit, none(), none(), None),
            item("m1", 0.75, Asked, side(Some(1)), side(Some(1)), Some(1.0)),
        ];
        store.record_known_items(run, &items).await.unwrap();
        let summary = store.known_item_summary(run).await.unwrap();
        assert_eq!(summary.len(), 2, "{summary:?}");
        let (hybrid, vector) = (&summary[0], &summary[1]);
        assert_eq!(
            (hybrid.alpha, hybrid.items, hybrid.primary_first),
            (0.75, 1, 1)
        );
        assert_eq!(vector.alpha, 1.0);
        let counted = (
            vector.items,
            vector.asked,
            vector.embed_failed,
            vector.no_permit,
            vector.primary_errors,
            vector.shadow_errors,
            vector.both_answered,
        );
        assert_eq!(counted, (8, 6, 1, 1, 0, 1, 5));
        let firsts = (
            vector.primary_first,
            vector.shadow_first,
            vector.only_primary_first,
            vector.only_shadow_first,
            vector.primary_within_k,
            vector.shadow_within_k,
        );
        assert_eq!(firsts, (3, 2, 2, 1, 4, 3));
        let close = |got: Option<f64>, want: f64| {
            let got = got.expect("a mean over answered items");
            assert!((got - want).abs() < 1e-9, "{got} != {want}");
        };
        close(
            vector.primary_mrr,
            (1.0 + 1.0 + 1.0 + 1.0 / 3.0 + 0.0) / 5.0,
        );
        close(vector.shadow_mrr, (1.0 + 0.5 + 0.0 + 1.0 + 0.0) / 5.0);
        close(vector.mean_overlap, 0.6);
        close(vector.mean_rbo, 0.6);
        close(vector.mean_jaccard, 0.6);
        close(vector.top1_rate, 1.0 / 5.0);
    })
    .await;
}

/// More items than one insert takes are all written.
#[tokio::test]
async fn items_past_one_batch_are_all_written() {
    with_store(|store, _pool| async move {
        let run = store
            .claim_known_item_run(&claim("memories", day(20)), STALE)
            .await
            .unwrap()
            .unwrap();
        let items: Vec<KnownItemRow> = (0..SINK_BATCH * 2 + 7)
            .map(|i| {
                item(
                    &format!("m{i}"),
                    1.0,
                    KnownItemStatus::Asked,
                    side(Some(1)),
                    side(Some(1)),
                    Some(1.0),
                )
            })
            .collect();
        store.record_known_items(run, &items).await.unwrap();
        let summary = store.known_item_summary(run).await.unwrap();
        assert_eq!(summary[0].items, (SINK_BATCH * 2 + 7) as i64);
    })
    .await;
}

/// Runs started before the cutoff go, with their items; later ones stay.
#[tokio::test(flavor = "multi_thread")]
async fn retention_deletes_runs_past_their_days_with_their_items() {
    with_store(|store, pool| async move {
        let old = store
            .claim_known_item_run(&claim("memories", day(1)), STALE)
            .await
            .unwrap()
            .unwrap();
        let kept = store
            .claim_known_item_run(&claim("memories", day(2)), STALE)
            .await
            .unwrap()
            .unwrap();
        for run in [old, kept] {
            store
                .record_known_items(
                    run,
                    &[item("m", 1.0, KnownItemStatus::Asked, side(Some(1)), side(Some(1)), None)],
                )
                .await
                .unwrap();
        }
        pool.get()
            .await
            .unwrap()
            .execute(
                "UPDATE shadow_known_item_runs SET started_at = now() - interval '181 days' WHERE id = $1",
                &[&old],
            )
            .await
            .unwrap();
        let before = Utc::now() - Days::days(i64::from(KNOWN_ITEMS_RETENTION_DAYS));
        assert_eq!(store.prune_known_item_runs(before).unwrap(), 1);
        let conn = pool.get().await.unwrap();
        let runs: Vec<i64> = conn
            .query("SELECT id FROM shadow_known_item_runs ORDER BY id", &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert_eq!(runs, vec![kept]);
        let items: i64 = conn
            .query_one("SELECT count(*) FROM shadow_known_items", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(items, 1, "the old run's items outlived it");
    })
    .await;
}

async fn put_artifact(pool: &PostgresPool, id: &str, title: &str, content: &str) {
    pool.get()
        .await
        .unwrap()
        .execute(
            "INSERT INTO artifacts (id, artifact_type, title, content, owner_id, created_by,
                 last_modified_by, created_at, updated_at)
             VALUES ($1, 'document', $2, $3, 'owner', 'owner', 'owner', now(), now())",
            &[&id, &title, &content],
        )
        .await
        .unwrap();
}

/// A draw reads every artifact a page at a time, in order of id; the judge
/// reads those it asks for, their content cut short.
#[tokio::test]
async fn artifacts_are_read_a_page_at_a_time_and_by_id() {
    with_store(|store, pool| async move {
        for i in [3, 1, 2, 5, 4] {
            put_artifact(
                &pool,
                &format!("a{i}"),
                &format!("title {i}"),
                "some long content",
            )
            .await;
        }
        let first = store.artifact_page(None, 2).await.unwrap();
        let ids = |page: &[ArtifactText]| page.iter().map(|a| a.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&first), ["a1", "a2"]);
        assert_eq!(
            (first[0].title.as_str(), first[0].content.as_str()),
            ("title 1", "some long content")
        );
        assert_eq!(
            ids(&store.artifact_page(Some("a2"), 2).await.unwrap()),
            ["a3", "a4"]
        );
        assert_eq!(
            ids(&store.artifact_page(Some("a4"), 2).await.unwrap()),
            ["a5"]
        );
        let got = store
            .artifacts_by_id(&["a4".into(), "gone".into(), "a2".into()], 4)
            .await
            .unwrap();
        let mut got: Vec<(String, String)> = got.into_iter().map(|a| (a.id, a.content)).collect();
        got.sort();
        assert_eq!(
            got,
            [("a2".into(), "some".into()), ("a4".into(), "some".into())]
        );
    })
    .await;
}

/// The judge's grades are summed over the items it judged, and counted by
/// day across that day's runs, so a run made again does not reset its cap.
#[tokio::test]
async fn judged_items_are_summed_and_counted_by_day() {
    with_store(|store, _pool| async move {
        let judged = |memory: &str, ndcg: (f64, f64)| KnownItemRow {
            ndcg: Some(ndcg),
            ..item(
                memory,
                1.0,
                KnownItemStatus::Asked,
                side(Some(1)),
                side(Some(3)),
                Some(0.5),
            )
        };
        let first = store
            .claim_known_item_run(&claim("artifacts", day(20)), STALE)
            .await
            .unwrap()
            .unwrap();
        store
            .record_known_items(
                first,
                &[
                    judged("a1", (1.0, 0.5)),
                    judged("a2", (0.5, 0.25)),
                    item(
                        "a3",
                        1.0,
                        KnownItemStatus::Asked,
                        side(Some(6)),
                        side(Some(1)),
                        Some(0.5),
                    ),
                ],
            )
            .await
            .unwrap();
        store
            .finish_known_item_run(first, KnownItemOutcome::Incomplete, None)
            .await
            .unwrap();
        let again = store
            .claim_known_item_run(&claim("artifacts", day(20)), STALE)
            .await
            .unwrap()
            .unwrap();
        store
            .record_known_items(again, &[judged("a4", (1.0, 1.0))])
            .await
            .unwrap();
        let other_day = store
            .claim_known_item_run(&claim("artifacts", day(21)), STALE)
            .await
            .unwrap()
            .unwrap();
        store
            .record_known_items(other_day, &[judged("a5", (1.0, 1.0))])
            .await
            .unwrap();
        assert_eq!(store.judged_on("artifacts", day(20)).await.unwrap(), 3);
        assert_eq!(store.judged_on("memories", day(20)).await.unwrap(), 0);

        let summary = store.known_item_summary(first).await.unwrap();
        let s = &summary[0];
        assert_eq!((s.judged, s.primary_within_5, s.shadow_within_5), (2, 2, 3));
        let close = |got: Option<f64>, want: f64| {
            assert!((got.unwrap() - want).abs() < 1e-9, "{got:?} != {want}")
        };
        close(s.primary_ndcg, 0.75);
        close(s.shadow_ndcg, 0.375);
    })
    .await;
}

/// A side that found the drawn memory only by a duplicate (another memory
/// of the same content) counts as finding it, and the finds by duplicate
/// are counted apart; the summary is kept apart by population and mode.
#[tokio::test]
async fn finds_by_duplicate_are_counted_apart_and_populations_and_modes_kept_apart() {
    with_store(|store, _pool| async move {
        let run = store
            .claim_known_item_run(&claim("memories", day(20)), STALE)
            .await
            .unwrap()
            .unwrap();
        use KnownItemStatus::Asked;
        let by_duplicate = |rank| KnownItemSide {
            by_duplicate: true,
            ..side(Some(rank))
        };
        let in_ = |population, mode, row: KnownItemRow| KnownItemRow {
            population,
            mode,
            ..row
        };
        let items = vec![
            item("m1", 1.0, Asked, side(Some(1)), side(Some(1)), Some(1.0)),
            item("m2", 1.0, Asked, by_duplicate(1), side(Some(1)), Some(1.0)),
            item("m3", 1.0, Asked, side(Some(1)), by_duplicate(2), Some(0.5)),
            in_(
                Population::Scoped,
                QueryMode::Hard,
                item("m1", 1.0, Asked, side(Some(2)), side(None), Some(0.5)),
            ),
            in_(
                Population::Unscoped,
                QueryMode::Window,
                item("u1", 1.0, Asked, side(Some(1)), side(Some(1)), Some(1.0)),
            ),
        ];
        store.record_known_items(run, &items).await.unwrap();
        let summary = store.known_item_summary(run).await.unwrap();
        let keys: Vec<(Population, QueryMode, f32, i64)> = summary
            .iter()
            .map(|s| (s.population, s.mode, s.alpha, s.items))
            .collect();
        assert_eq!(
            keys,
            vec![
                (Population::Scoped, QueryMode::Window, 1.0, 3),
                (Population::Scoped, QueryMode::Hard, 1.0, 1),
                (Population::Unscoped, QueryMode::Window, 1.0, 1),
            ]
        );
        let window = &summary[0];
        assert_eq!(
            (
                window.primary_first,
                window.shadow_first,
                window.primary_found_by_duplicate,
                window.shadow_found_by_duplicate,
            ),
            (3, 2, 1, 1)
        );
        assert_eq!(
            (summary[1].primary_first, summary[1].shadow_within_k),
            (0, 0)
        );
    })
    .await;
}

/// An artifact the serving index or the mirror has yet to take is read as
/// pending, so that a draw leaves it out.
#[tokio::test]
async fn artifacts_still_pending_are_read_as_such() {
    with_store(|store, pool| async move {
        for id in ["a1", "a2", "a3"] {
            put_artifact(&pool, id, "t", "c").await;
        }
        let conn = pool.get().await.unwrap();
        // Writing an artifact notes it for the serving index (V023's
        // trigger): clear those, then note one for each index by hand.
        conn.batch_execute("DELETE FROM artifact_index_pending")
            .await
            .unwrap();
        conn.execute(
            "INSERT INTO artifact_index_pending (artifact_id, text_hash) VALUES ('a1', 'h')",
            &[],
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO artifact_index_pending_mirror (artifact_id, text_hash) VALUES ('a3', 'h')",
            &[],
        )
        .await
        .unwrap();
        let page = store.artifact_page(None, 10).await.unwrap();
        let pending: Vec<(String, bool)> = page.into_iter().map(|a| (a.id, a.pending)).collect();
        assert_eq!(
            pending,
            [
                ("a1".into(), true),
                ("a2".into(), false),
                ("a3".into(), true)
            ]
        );
    })
    .await;
}

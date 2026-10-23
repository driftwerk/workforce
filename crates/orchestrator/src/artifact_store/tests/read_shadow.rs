//! Shadow mode's reads of the artifact index (DW-676, PR 5): each search
//! the serving index ranks by meaning is asked of the mirror too, in the
//! background, and compared.

use std::sync::Arc;
use std::time::{Duration, Instant};

use workforce_common::shadow::{MemorySink, Outcome, Shadow, ShadowSettings};

use super::*;

fn settings(permits: usize) -> ShadowSettings {
    ShadowSettings {
        sample: 1.0,
        permits,
        deadline: Duration::from_secs(2),
        trip_after: 3,
        rest: Duration::from_secs(30),
        k: 10,
        rbo_p: 0.9,
    }
}

/// A serving index and a mirror, both filled and in step, the serving
/// store's reads shadowed by the mirror through `reader` (the mirror's
/// index as shadow reads ask it) into a sink the test reads.
struct Shadowed {
    store: Arc<KnowledgeStore>,
    weaviate: Arc<Held>,
    reader: Arc<Held>,
    shadow: Arc<Shadow>,
    sink: Arc<MemorySink>,
    _mirror: MirrorStore,
}

async fn shadowed(pool: &PostgresPool, permits: usize) -> Shadowed {
    let weaviate = Held::on(pool);
    let vellev = Held::on(pool);
    let reader = Held::on(pool);
    let store = KnowledgeStore::new(pool.clone(), Some(weaviate.clone()))
        .await
        .unwrap();
    stored(&store, 3).await;
    let mirror = store
        .open_mirror(pool.clone(), vellev.clone(), Duration::from_secs(3600))
        .await
        .unwrap();
    store.keep_index_complete(QUICK);
    mirror.keep_index_complete(QUICK);
    until("both indexes filled", || {
        weaviate.is_complete()
            && vellev.is_complete()
            && weaviate.ids_held().len() == 3
            && vellev.ids_held().len() == 3
    })
    .await;
    until("the mirror in step", || {
        mirror.index_state().search == Some("index")
    })
    .await;
    until_none_pending(&store).await;
    let sink = Arc::new(MemorySink::default());
    let shadow = Arc::new(Shadow::new("artifacts", settings(permits), sink.clone()).unwrap());
    store
        .shadow_reads(&mirror, reader.clone(), Arc::clone(&shadow))
        .unwrap();
    *weaviate.ranking.lock().unwrap() = vec![
        ("a000".into(), 0.9),
        ("a001".into(), 0.8),
        ("a002".into(), 0.7),
    ];
    *reader.ranking.lock().unwrap() = vec![("a001".into(), 0.6), ("a000".into(), 0.5)];
    Shadowed {
        store,
        weaviate,
        reader,
        shadow,
        sink,
        _mirror: mirror,
    }
}

async fn search(store: &KnowledgeStore) -> Searched {
    store
        .search_told(
            "rust",
            &Caller::user("alice"),
            &ArtifactFilter::default(),
            5,
        )
        .await
        .unwrap()
}

/// A search the serving index ranks by meaning is asked of the mirror
/// with the same question: the primary's raw ranking (before any ACL
/// read) beside the mirror's, compared, its question kept as a hash.
#[tokio::test(flavor = "multi_thread")]
async fn a_search_by_meaning_is_asked_of_the_mirror_and_compared() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let s = shadowed(&pool, 4).await;
            let found = search(&s.store).await;
            assert!(found.by_keyword.is_none(), "{:?}", found.by_keyword);
            until("compared", || s.sink.len() == 1).await;
            let c = s.sink.taken().remove(0);
            assert_eq!(c.store, "artifacts");
            assert_eq!(c.primary.ranked.ids, ["a000", "a001", "a002"]);
            assert_eq!(c.primary.ranked.top_score, Some(0.9));
            assert_eq!(
                c.primary.query_hash.as_deref(),
                Some(blake3::hash(b"rust").to_hex().as_str())
            );
            assert!(c
                .primary
                .labels
                .contains(&("same_model", "false".to_string())));
            assert!(c
                .primary
                .labels
                .contains(&("mirror_in_step", "true".to_string())));
            let Outcome::Answered { ranked, metrics } = c.outcome else {
                panic!("{:?}", c.outcome)
            };
            assert_eq!(ranked.ids, ["a001", "a000"]);
            assert!(!metrics.top1);
            assert!(s.reader.searches.load(Ordering::SeqCst) == 1);
        },
    )
    .await;
}

/// A search that went by keyword is not asked of the mirror: only
/// searches by meaning are compared.
#[tokio::test(flavor = "multi_thread")]
async fn a_search_by_keyword_is_not_shadowed() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let s = shadowed(&pool, 4).await;
            s.weaviate.complete.store(false, Ordering::SeqCst);
            let found = search(&s.store).await;
            assert!(found.by_keyword.is_some());
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(s.sink.is_empty());
            assert_eq!(s.shadow.counts().offered, 0);
            assert_eq!(s.reader.searches.load(Ordering::SeqCst), 0);
        },
    )
    .await;
}

/// While the mirror is not in step, as its keeper last saw it, a search is
/// not compared: the mirror would be judged on artifacts it does not hold.
#[tokio::test(flavor = "multi_thread")]
async fn a_search_while_the_mirror_is_not_in_step_is_not_shadowed() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let s = shadowed(&pool, 4).await;
            s._mirror
                .0
                .index
                .update(|state| state.search = Some("keyword"));
            let found = search(&s.store).await;
            assert!(found.by_keyword.is_none());
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(s.sink.is_empty());
            assert_eq!(s.shadow.counts().offered, 0);
        },
    )
    .await;
}

/// A mirror that hangs or fails changes nothing a user sees: the same
/// hits, as soon as without a shadow, and the serving index's keeping as
/// it was. Its failure is recorded as such.
#[tokio::test(flavor = "multi_thread")]
async fn a_hung_or_failing_mirror_leaves_the_users_search_as_it_was() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let s = shadowed(&pool, 4).await;
            let ids = |found: &Searched| -> Vec<String> {
                found
                    .hits
                    .iter()
                    .map(|hit| hit.artifact.id.clone())
                    .collect()
            };
            let before = search(&s.store).await;
            until("the first compared", || s.sink.len() == 1).await;

            s.reader.hung.store(true, Ordering::SeqCst);
            let began = Instant::now();
            let hung = search(&s.store).await;
            assert!(
                began.elapsed() < Duration::from_millis(500),
                "{:?}",
                began.elapsed()
            );
            assert_eq!(ids(&hung), ids(&before));
            assert_eq!(hung.by_keyword, None);

            s.reader.hung.store(false, Ordering::SeqCst);
            s.reader.deaf.store(true, Ordering::SeqCst);
            let failed = search(&s.store).await;
            assert_eq!(ids(&failed), ids(&before));
            until("the failure recorded", || s.shadow.counts().failed == 1).await;
            // The serving index is still relied on: neither the mirror's
            // failure nor its hang marked it incomplete or sent search by
            // keyword (its keeper's own rounds go on meanwhile).
            let serving = s.store.index_state();
            assert_eq!(serving.complete, Some(true), "{serving:?}");
            assert_ne!(serving.search, Some("keyword"), "{serving:?}");
            assert_ne!(serving.in_step, Some(false), "{serving:?}");
            assert_eq!(search(&s.store).await.by_keyword, None);
        },
    )
    .await;
}

/// Shadow reads are bounded by the kernel's permits: with one, a search
/// while the first shadow read still runs is not asked of the mirror.
#[tokio::test(flavor = "multi_thread")]
async fn shadow_reads_are_bounded_by_the_kernels_permits() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let s = shadowed(&pool, 1).await;
            s.reader.hung.store(true, Ordering::SeqCst);
            search(&s.store).await;
            until("the first running", || {
                s.reader.searches.load(Ordering::SeqCst) == 1
            })
            .await;
            search(&s.store).await;
            search(&s.store).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            let counts = s.shadow.counts();
            assert_eq!((counts.started, counts.busy), (1, 2), "{counts:?}");
            assert_eq!(s.reader.searches.load(Ordering::SeqCst), 1);
        },
    )
    .await;
}

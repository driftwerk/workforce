//! Shadow mode for memories end to end (DW-676, PR 8): a hub configured
//! with `[memory] backend = "shadow"` opens memories, keeps the replica,
//! shadows recalls and erases, against a real Weaviate, a real vellev node
//! and Postgres, the hub embedding with a text-embeddings-inference stand-in.
//!
//! Needs `WORKFORCE_TEST_WEAVIATE_URL` (a Weaviate it may make collections
//! in) besides a node (`VELLEV_TEST_URL`); skipped without it, unless
//! `WORKFORCE_TEST_WEAVIATE_REQUIRED=1`. As in production, that Weaviate
//! must have the `text2vec-transformers` module, for the collection
//! memories are moved from (its inference service need only answer
//! `/.well-known/ready`: nothing is vectorized by Weaviate here).
//!
//! CI's `vellev-node` job runs it against Weaviate's own binary, at a
//! pinned version, with `WORKFORCE_TEST_WEAVIATE_REQUIRED=1`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use workforce_common::postgres::PostgresConfig;
use workforce_common::settings::{EmbeddingsProvider, WorkforceConfig, SHADOWED};
use workforce_common::telemetry_postgres::PostgresTelemetryStore;
use workforce_core::types::AgentId;
use workforce_memory::types::{Memory, MemoryFilter, MemoryLayer};

/// The Weaviate to use, or `None` to skip.
fn weaviate() -> Option<String> {
    match std::env::var("WORKFORCE_TEST_WEAVIATE_URL") {
        Ok(url) => Some(url),
        Err(_) if std::env::var("WORKFORCE_TEST_WEAVIATE_REQUIRED").as_deref() == Ok("1") => {
            panic!("WORKFORCE_TEST_WEAVIATE_REQUIRED=1 and no WORKFORCE_TEST_WEAVIATE_URL")
        }
        Err(_) => {
            eprintln!("skipping: WORKFORCE_TEST_WEAVIATE_URL names no Weaviate");
            None
        }
    }
}

/// A text-embeddings-inference server's `/embed`, embedding each text by
/// its words; counting the texts it embeds.
async fn embedder() -> (String, Arc<AtomicUsize>) {
    use axum::{routing::post, Json, Router};
    let embedded = Arc::new(AtomicUsize::new(0));
    let counting = Arc::clone(&embedded);
    let app = Router::new().route(
        "/embed",
        post(move |Json(body): Json<serde_json::Value>| {
            let counting = Arc::clone(&counting);
            async move {
                let vectors: Vec<Vec<f32>> = body["inputs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|text| {
                        counting.fetch_add(1, Ordering::SeqCst);
                        words(text.as_str().unwrap_or_default())
                    })
                    .collect();
                Json(vectors)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (url, embedded)
}

/// A vector of 16 by the words of `text`, never all zero.
fn words(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0; 16];
    vector[0] = 0.01;
    for word in text.split_whitespace() {
        let hash = word
            .to_lowercase()
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
            });
        vector[(hash % 16) as usize] += 1.0;
    }
    vector
}

/// A word that is not `word` but that [`words`]
/// embeds as it embeds `word`.
fn alike(word: &str) -> String {
    let want = words(word);
    (0..)
        .map(|n| format!("zq{n}"))
        .find(|other| words(other) == want)
        .expect("some word lands where it does")
}

/// Wait up to `limit` for `done`, asking every half second.
async fn until<F, Fut>(limit: Duration, what: &str, mut done: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let began = tokio::time::Instant::now();
    while !done().await {
        assert!(began.elapsed() < limit, "{what} within {limit:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hub_shadowing_memories_keeps_the_replica_compares_recalls_and_erases() {
    let Some(weaviate) = weaviate() else {
        return;
    };
    let Some(node) = workforce_vellev::testing::Node::fresh().await else {
        return;
    };
    let ran = Arc::new(AtomicUsize::new(0));
    let reached = Arc::clone(&ran);
    let pg: Arc<Mutex<Option<PostgresConfig>>> = Arc::default();
    let seen = Arc::clone(&pg);
    workforce_postgres::test_support::with_test_pool(
        move |cfg| {
            *seen.lock().unwrap() = Some(cfg.clone());
            PostgresTelemetryStore::build_pool(cfg)
        },
        |pool| async move {
            let (embed_url, embedded) = embedder().await;
            let mut config = WorkforceConfig::default();
            config.memory.backend = SHADOWED.into();
            config.weaviate.enabled = true;
            config.weaviate.url = weaviate;
            config.vellev.url = node.url.clone();
            config.vellev.api_key = Some(node.key.clone());
            config.vellev.tenant = node.tenant.clone();
            config.embeddings.provider = EmbeddingsProvider::Tei;
            config.embeddings.url = Some(embed_url);
            config.embeddings.model = Some("words".into());
            // Not the defaults, so that a store built apart from the config
            // would show.
            config.embeddings.alpha = 0.6;
            config.embeddings.rerank_candidates = 12;
            config.embeddings.score_map = vec![[0.0, 0.0], [1.0, 0.9]];
            config.embeddings.collection =
                Some(format!("ShadowE2e{}", uuid::Uuid::new_v4().simple()));
            config.postgres = pg.lock().unwrap().clone();
            config.check_memory_backend().expect("accepted");

            let store = crate::state::resolve_memory_store(&config).await.unwrap();
            until(
                Duration::from_secs(60),
                "memories switched to the hub's model",
                || {
                    let store = Arc::clone(&store);
                    async move { store.query_model().as_deref() == Some("words") }
                },
            )
            .await;

            // As in production, where memories switched long before:
            // the reconcile's first run (after the leader's wiring) finds
            // them switched and makes the replica's collection, which the
            // applier writes into. Before the switch it would refuse, and
            // not run again for `memory_reconcile_secs`.
            let drain = CancellationToken::new();
            let shadow = crate::shadow::shared_store(&config).await.unwrap();
            let tasks = crate::memory_shadow::start(&config, shadow, None, drain.clone()).await;
            assert_eq!(tasks.len(), 2, "the reconcile and the applier");

            // A write is noted, and the leader's applier copies it.
            let id = store
                .remember(
                    Memory::new(
                        "otters hold hands asleep",
                        MemoryLayer::Episodic,
                        AgentId::new("agent"),
                    )
                    .with_identity_id("bob"),
                )
                .await
                .unwrap();
            // Noted by the hub's noter: `resolve_memory_store` opens
            // memories as the hub does (`MemoryOpening::hub`), and the
            // shadowed store is given the process's noter. Without it the
            // reconcile would still copy the memory, in time; this is what
            // shows the write was noted. (This is the only test in the
            // crate that opens a shadowed store as the hub, so the
            // process's one noter is this test's.)
            until(Duration::from_secs(30), "the write noted", || async {
                crate::memory_erasure::noter_counts()
                    .is_some_and(|counts| counts.written.load(Ordering::SeqCst) >= 1)
            })
            .await;
            let (replica, _) = crate::embeddings::shadow_stores(&config).await.unwrap();
            // The two stores compared rank alike (A13): the one the user is
            // answered from, as the hub opened it, and the replica.
            let settings = store.recall_settings().expect("embedded for");
            assert_eq!(settings.alpha, 0.6);
            assert_eq!(settings.rerank_candidates, 12);
            assert_eq!(replica.recall_settings(), Some(settings));
            until(Duration::from_secs(90), "the memory in the replica", || {
                let (replica, id) = (Arc::clone(&replica), id.0.clone());
                // The replica's collection is made by the first copy to it.
                async move {
                    matches!(
                        replica.objects_of_memory(&id, false).await,
                        Ok(objects) if !objects.is_empty()
                    )
                }
            })
            .await;

            // A recall is Weaviate's, and is compared with the replica's:
            // embedded once, by the hub, for both stores (A5), each built
            // by the one constructor (`shadow_stores`); the replica, given
            // that vector, finds the same memory first.
            let bob = MemoryFilter {
                identity_id: Some("bob".into()),
                ..MemoryFilter::default()
            };
            let before = embedded.load(Ordering::SeqCst);
            let hits = store.recall("otters asleep", bob.clone()).await.unwrap();
            assert_eq!(hits[0].memory.id, id);
            until(Duration::from_secs(30), "the recall compared", || {
                let pool = pool.clone();
                async move {
                    let conn = pool.get().await.unwrap();
                    let row = conn
                        .query_one(
                            "SELECT count(*) FROM shadow_comparisons \
                             WHERE store = 'memories' AND outcome = 'answered'",
                            &[],
                        )
                        .await
                        .unwrap();
                    row.get::<_, i64>(0) > 0
                }
            })
            .await;
            assert_eq!(
                embedded.load(Ordering::SeqCst) - before,
                1,
                "one embedding for the recall and its shadow"
            );
            let conn = pool.get().await.unwrap();
            let row = conn
                .query_one(
                    "SELECT top1, shadow_embed_ms IS NULL FROM shadow_comparisons \
                     WHERE store = 'memories' AND outcome = 'answered'",
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(
                row.get::<_, Option<bool>>(0),
                Some(true),
                "the same memory first"
            );
            assert!(row.get::<_, bool>(1), "the replica was handed the vector");
            drop(conn);

            // The replica searched by that vector, not by keywords alone: a
            // question sharing no word with the memory, but embedded (by
            // `words`) as its words are, is found by meaning or not at all.
            // A replica not handed the vector, falling back to keywords,
            // finds nothing, and the comparison says so.
            let by_meaning = format!("{} {}", alike("otters"), alike("asleep"));
            let hits = store.recall(&by_meaning, bob.clone()).await.unwrap();
            assert_eq!(hits[0].memory.id, id, "Weaviate found it by meaning");
            until(
                Duration::from_secs(30),
                "the second recall compared",
                || {
                    let pool = pool.clone();
                    async move {
                        let conn = pool.get().await.unwrap();
                        let row = conn
                            .query_one(
                                "SELECT count(*) FROM shadow_comparisons \
                             WHERE store = 'memories' AND outcome = 'answered'",
                                &[],
                            )
                            .await
                            .unwrap();
                        row.get::<_, i64>(0) > 1
                    }
                },
            )
            .await;
            let conn = pool.get().await.unwrap();
            let row = conn
                .query_one(
                    "SELECT shadow_count, top1, shadow_embed_ms IS NULL \
                     FROM shadow_comparisons \
                     WHERE store = 'memories' AND outcome = 'answered' \
                     ORDER BY id DESC LIMIT 1",
                    &[],
                )
                .await
                .unwrap();
            assert!(
                row.get::<_, Option<i32>>(0).unwrap_or(0) >= 1
                    && row.get::<_, Option<bool>>(1) == Some(true)
                    && row.get::<_, bool>(2),
                "the replica found by meaning, with the hub's vector, what shares no word \
                 with the question (shadow_count {:?}, top1 {:?})",
                row.get::<_, Option<i32>>(0),
                row.get::<_, Option<bool>>(1),
            );
            drop(conn);

            // A purge reaches the replica in the same call.
            store
                .purge_memories(std::slice::from_ref(&id))
                .await
                .unwrap();
            assert!(replica
                .objects_of_memory(&id.0, false)
                .await
                .unwrap()
                .is_empty());
            assert!(store.get(&id).await.unwrap().is_none());

            drain.cancel();
            for task in tasks {
                task.await.unwrap();
            }
            reached.fetch_add(1, Ordering::SeqCst);
        },
    )
    .await;
    assert!(
        ran.load(Ordering::SeqCst) == 1 || std::env::var("WORKFORCE_SKIP_POSTGRES_TESTS").is_ok()
    );
}

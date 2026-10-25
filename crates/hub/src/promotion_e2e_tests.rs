//! Promotion end to end (DW-676, PR 11), against a real vellev node and
//! Postgres, and for memories a real Weaviate: what was shadowed is
//! promoted, served from vellev with nothing of it asked of Weaviate, and
//! rolled back whole.
//!
//! Needs a node (`VELLEV_TEST_URL`); the memories' test also needs
//! `WORKFORCE_TEST_WEAVIATE_URL`, and is skipped without it unless
//! `WORKFORCE_TEST_WEAVIATE_REQUIRED=1`, as the shadow's own end-to-end
//! test is.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use workforce_common::postgres::PostgresConfig;
use workforce_common::settings::{EmbeddingsProvider, WorkforceConfig, SHADOWED};
use workforce_common::telemetry_postgres::PostgresTelemetryStore;
use workforce_core::types::AgentId;
use workforce_memory::types::{Memory, MemoryFilter, MemoryLayer};
use workforce_orchestrator::artifact_store::{
    Artifact, ArtifactBackend, ArtifactFilter, ArtifactType, Caller, KnowledgeStore, Visibility,
};

use super::*;

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
/// its words.
async fn embedder() -> String {
    use axum::{routing::post, Json, Router};
    let app = Router::new().route(
        "/embed",
        post(|Json(body): Json<serde_json::Value>| async move {
            let vectors: Vec<Vec<f32>> = body["inputs"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|text| words(text.as_str().unwrap_or_default()))
                .collect();
            Json(vectors)
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
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

/// A local address that counts the connections made to it: where Weaviate
/// is said to be, once nothing should ask it.
async fn counted() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let reached = Arc::new(AtomicUsize::new(0));
    let counting = Arc::clone(&reached);
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            counting.fetch_add(1, Ordering::SeqCst);
            drop(conn);
        }
    });
    (url, reached)
}

struct Verdicts(Verdict);

#[async_trait]
impl VerdictSource for Verdicts {
    async fn verdict(&self, _store: &str) -> Result<Verdict> {
        Ok(self.0.clone())
    }
}

fn quiet(_: &str) {}

/// Run `test` with a fresh database and the config of it.
async fn on_postgres<F, Fut>(test: F)
where
    F: FnOnce(PostgresConfig, PostgresPool) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let seen: Arc<Mutex<Option<PostgresConfig>>> = Arc::default();
    let keep = Arc::clone(&seen);
    workforce_postgres::test_support::with_test_pool(
        move |cfg| {
            *keep.lock().unwrap() = Some(cfg.clone());
            PostgresTelemetryStore::build_pool(cfg)
        },
        |pool| async move {
            let pg = seen.lock().unwrap().clone().expect("built");
            test(pg, pool).await;
        },
    )
    .await;
}

/// Memories shadowed for a week, then promoted: served from vellev, a
/// write and an erasure taken there with nothing asked of Weaviate, and a
/// process on the shadow config refused; then rolled back, Weaviate taking
/// both, and the shadow config opening again. Then a hub restarts on it,
/// catches up and reconciles: the memory erased on vellev comes back
/// nowhere, though the store memories switched from still holds it (B1).
#[tokio::test(flavor = "multi_thread")]
async fn memories_promoted_are_served_from_vellev_alone_and_rolled_back_whole() {
    let Some(weaviate) = weaviate() else {
        return;
    };
    let Some(node) = workforce_vellev::testing::Node::fresh().await else {
        return;
    };
    on_postgres(|pg, pool| async move {
        let mut config = WorkforceConfig::default();
        config.memory.backend = SHADOWED.into();
        config.embeddings.keep_previous = false;
        config.weaviate.enabled = true;
        config.weaviate.url = weaviate;
        config.vellev.url = node.url.clone();
        config.vellev.api_key = Some(node.key.clone());
        config.vellev.tenant = node.tenant.clone();
        config.embeddings.provider = EmbeddingsProvider::Tei;
        config.embeddings.url = Some(embedder().await);
        config.embeddings.model = Some("words".into());
        config.embeddings.collection = Some(format!("Promote{}", uuid::Uuid::new_v4().simple()));
        config.postgres = Some(pg);
        config.check_memory_backend().expect("accepted");

        // Weaviate serving, switched long ago, as in production; written
        // to as a CLI write is, which the reconcile alone carries over.
        let (_, serving) = crate::embeddings::shadow_stores(&config).await.unwrap();
        serving.ensure_collection().await.unwrap();
        serving.mark_switched().await.unwrap();
        let agent = AgentId::new("agent");
        let bob = |text: &str| {
            Memory::new(text, MemoryLayer::Episodic, agent.clone()).with_identity_id("bob")
        };
        let kept = serving
            .remember(bob("otters hold hands asleep"))
            .await
            .unwrap();
        let erased = serving.remember(bob("badgers dig at night")).await.unwrap();
        // As the store memories switched from (Weaviate's WorkforceMemory)
        // holds it: nothing erases there once memories are promoted.
        let stale = serving.objects_of_memory(&erased.0, true).await.unwrap();
        assert_eq!(stale.len(), 1);

        promote_memories(&config, Arc::new(Verdicts(Verdict::Pass)), &quiet)
            .await
            .unwrap();
        let err = guard_memories(&config).await.expect_err("promoted");
        assert!(format!("{err:#}").contains("rollback"), "{err:#}");
        // Every opener asks first: the hub's, and a side process's.
        let opened = crate::state::resolve_memory_store(&config).await;
        assert!(
            format!("{:#}", opened.err().expect("refused")).contains("rollback"),
            "the hub's opener"
        );
        let base = crate::embeddings::base_store(&config).unwrap();
        let opened = crate::embeddings::switched_memory_store(&config, base).await;
        assert!(
            format!("{:#}", opened.err().expect("refused")).contains("rollback"),
            "a side process's opener"
        );

        // Served from vellev, with Weaviate said to be where nothing
        // answers but a counter.
        let (nowhere, reached) = counted().await;
        let mut primary = config.clone();
        primary.memory.backend = "vellev".into();
        primary.weaviate.url = nowhere;
        let store = crate::state::resolve_memory_store(&primary).await.unwrap();
        let only_bob = MemoryFilter {
            identity_id: Some("bob".into()),
            ..MemoryFilter::default()
        };
        let hits = store
            .recall("otters asleep", only_bob.clone())
            .await
            .unwrap();
        assert_eq!(hits[0].memory.id, kept);
        let written = store
            .remember(bob("vellev took this one after"))
            .await
            .unwrap();
        assert_eq!(
            store
                .purge_memories(std::slice::from_ref(&erased))
                .await
                .unwrap(),
            1
        );
        // The first collection the vellev hub keeps as its previous is
        // empty: reading it for an erasure by identity does not fail.
        assert_eq!(
            store.identity_counts(&agent).await.unwrap().get("bob"),
            Some(&2)
        );
        assert_eq!(
            reached.load(Ordering::SeqCst),
            0,
            "nothing of memories asks Weaviate once they are promoted"
        );

        rollback_memories(&config, &quiet).await.unwrap();
        guard_memories(&config).await.unwrap();
        let carried = serving.objects_of_memory(&written.0, false).await.unwrap();
        assert_eq!(carried.len(), 1, "written on vellev, now in Weaviate");
        assert!(
            serving
                .objects_of_memory(&erased.0, false)
                .await
                .unwrap()
                .is_empty(),
            "erased on vellev, and so in Weaviate"
        );
        let first = crate::embeddings::shadow_v1_store(&config).unwrap();
        assert!(
            !first.exists().await.unwrap(),
            "the first collection the vellev hub made is dropped, as the shadow refuses it"
        );
        let replica = embedded_collection(&config).unwrap();
        assert!(!replica.promoted().await.unwrap());
        // The promotion is history: no hub may keep the store memories
        // switched from again, rolled back or not.
        let mut keeping = config.clone();
        keeping.embeddings.keep_previous = true;
        let err = guard_memories(&keeping).await.expect_err("kept again");
        assert!(
            format!("{err:#}").contains("keep_previous = false"),
            "{err:#}"
        );

        // A hub restarts on the shadow config. The store memories switched
        // from still holds the erased memory; a vellev collection stands in
        // for Weaviate's WorkforceMemory, which cannot embed in tests.
        let previous = Arc::new(
            VellevStore::new(
                &node.url,
                &node.key,
                &node.tenant,
                &format!("B1Previous{}", uuid::Uuid::new_v4().simple()),
            )
            .unwrap(),
        );
        previous.ensure_collection().await.unwrap();
        // Embedded there by the node's own model, as Weaviate's own
        // vectorizer embeds WorkforceMemory: the hub's vectors are not kept.
        let stale: Vec<serde_json::Value> = stale
            .into_iter()
            .map(|mut object| {
                object.as_object_mut().map(|o| o.remove("vector"));
                object
            })
            .collect();
        workforce_memory::admin::SyncTarget::upsert(previous.as_ref(), &stale)
            .await
            .unwrap();
        crate::embeddings::CATCH_UP_EVERY_IN_TESTS.store(300, Ordering::SeqCst);
        let restarted = crate::embeddings::memory_store(
            &config,
            Arc::clone(&previous) as Arc<dyn workforce_memory::admin::Switchable>,
            None,
            std::time::Duration::from_secs(30),
        )
        .await
        .unwrap();
        // Several catch-ups' worth, then a reconcile into the replica.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let shadow = workforce_common::shadow_postgres::ShadowStore::with_pool(pool.clone())
            .await
            .unwrap();
        let reconciling = crate::memory_shadow::reconciling(&config, shadow)
            .await
            .unwrap();
        let run = reconciling.reconcile.once().await;
        assert_eq!(run.outcome.as_str(), "ok", "{:?}", run.error);
        assert!(
            serving
                .objects_of_memory(&erased.0, false)
                .await
                .unwrap()
                .is_empty(),
            "the erased memory is not caught up into Weaviate's served collection"
        );
        assert!(
            reconciling
                .replica
                .objects_of_memory(&erased.0, false)
                .await
                .unwrap()
                .is_empty(),
            "nor reconciled into vellev's replica"
        );
        assert!(restarted.get(&erased).await.unwrap().is_none());
    })
    .await;
}

/// With `[embeddings] keep_previous = false`, a hub opening memories never
/// asks the store they switched from: not to remember, forget, erase or
/// count, and not to catch up.
#[tokio::test(flavor = "multi_thread")]
async fn memories_opened_without_their_previous_store_never_ask_it() {
    let Some(weaviate) = weaviate() else {
        return;
    };
    // The replica the pair names, on a real node, as in shadow mode: run
    // where the shadow's other end-to-end tests run.
    let Some(node) = workforce_vellev::testing::Node::fresh().await else {
        return;
    };
    let mut config = WorkforceConfig::default();
    config.memory.backend = "weaviate".into();
    config.embeddings.keep_previous = false;
    config.weaviate.enabled = true;
    config.weaviate.url = weaviate;
    config.embeddings.provider = EmbeddingsProvider::Tei;
    config.embeddings.url = Some(embedder().await);
    config.embeddings.model = Some("words".into());
    config.embeddings.collection = Some(format!("NoPrevious{}", uuid::Uuid::new_v4().simple()));
    config.vellev.url = node.url.clone();
    config.vellev.api_key = Some(node.key.clone());
    config.vellev.tenant = node.tenant.clone();
    let (_, serving) = crate::embeddings::shadow_stores(&config).await.unwrap();
    serving.ensure_collection().await.unwrap();
    serving.mark_switched().await.unwrap();
    // WorkforceMemory, said to be where nothing answers but a counter.
    let (nowhere, reached) = counted().await;
    let previous = Arc::new(workforce_memory::weaviate::WeaviateStore::new(
        &nowhere, None,
    ));
    crate::embeddings::CATCH_UP_EVERY_IN_TESTS.store(300, Ordering::SeqCst);
    let store = crate::embeddings::memory_store(
        &config,
        previous as Arc<dyn workforce_memory::admin::Switchable>,
        None,
        std::time::Duration::from_secs(30),
    )
    .await
    .unwrap();
    let agent = AgentId::new("agent");
    let id = store
        .remember(
            Memory::new("otters", MemoryLayer::Episodic, agent.clone()).with_identity_id("bob"),
        )
        .await
        .unwrap();
    store.forget(&id).await.unwrap();
    store.identity_counts(&agent).await.unwrap();
    store.purge_identity(&agent, "bob").await.unwrap();
    store
        .purge_memories(std::slice::from_ref(&id))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let side = crate::embeddings::switched_memory_store(
        &config,
        Arc::new(workforce_memory::weaviate::WeaviateStore::new(
            &nowhere, None,
        )),
    )
    .await
    .unwrap();
    side.forget(&id).await.unwrap();
    assert_eq!(
        reached.load(Ordering::SeqCst),
        0,
        "the store memories switched from was asked"
    );
}

fn artifact(id: &str, title: &str) -> Artifact {
    Artifact::new(
        id,
        ArtifactType::Notes,
        title,
        format!("{title}, written down"),
        "alice",
        "alice",
        Visibility::Public,
        Vec::new(),
    )
}

/// An artifact index shadowed, then promoted: refused while a shadow hub's
/// lease holds, and on "too few to judge" without the owner's word; then
/// the mirror's noting ended, the index served from vellev with nothing
/// asked of Weaviate, and rolled back.
#[tokio::test(flavor = "multi_thread")]
async fn an_artifact_index_promoted_is_served_from_vellev_alone() {
    let Some(node) = workforce_vellev::testing::Node::fresh().await else {
        return;
    };
    on_postgres(|pg, pool| async move {
        let (nowhere, reached) = counted().await;
        let data = tempfile::tempdir().unwrap();
        let mut config = WorkforceConfig::default();
        config.artifacts.index_backend = SHADOWED.into();
        config.weaviate.enabled = true;
        config.weaviate.url = nowhere;
        config.vellev.url = node.url.clone();
        config.vellev.api_key = Some(node.key.clone());
        config.vellev.tenant = node.tenant.clone();
        config.postgres = Some(pg);
        config.hub.data_dir = data.path().display().to_string();

        let artifacts = KnowledgeStore::new(pool.clone(), None).await.unwrap();
        let conn = pool.get().await.unwrap();
        // As a hub keeping the mirror leaves it: leased, writes noted.
        conn.batch_execute("SELECT artifact_mirror_renew('90 seconds')")
            .await
            .unwrap();
        artifacts
            .create(&artifact("a1", "otter field notes"))
            .await
            .unwrap();
        artifacts
            .create(&artifact("a2", "badger survey"))
            .await
            .unwrap();

        let pass: Arc<dyn VerdictSource> = Arc::new(Verdicts(Verdict::Pass));
        let err = promote_artifacts(&config, Arc::clone(&pass), false, &quiet)
            .await
            .expect_err("leased");
        assert!(format!("{err:#}").contains("lease"), "{err:#}");
        // Every hub stopped, and the lease run out.
        conn.batch_execute(
            "UPDATE shadow_control SET mirrored_until = now() - interval '1 second'",
        )
        .await
        .unwrap();
        let too_few: Arc<dyn VerdictSource> = Arc::new(Verdicts(Verdict::TooFewToJudge));
        let err = promote_artifacts(&config, Arc::clone(&too_few), false, &quiet)
            .await
            .expect_err("too few");
        assert!(format!("{err:#}").contains("too few to judge"), "{err:#}");
        promote_artifacts(&config, too_few, true, &quiet)
            .await
            .unwrap();

        let facts = workforce_common::shadow_postgres::artifact_mirror_facts(&pool)
            .await
            .unwrap();
        assert_eq!(
            (facts.lease_held, facts.pending, facts.promoted),
            (false, 0, true)
        );
        assert!(vellev_artifacts(&config).unwrap().promoted().await.unwrap());

        let mut primary = config.clone();
        primary.artifacts.index_backend = "vellev".into();
        guard_artifacts(&primary).await.unwrap();
        let served = crate::serve::open_artifact_store(&primary)
            .await
            .expect("opened");
        let found = served
            .search_told(
                "otter field notes",
                &Caller::user("alice"),
                &ArtifactFilter::default(),
                5,
            )
            .await
            .unwrap();
        assert!(found.by_keyword.is_none(), "{:?}", found.by_keyword);
        assert_eq!(found.hits[0].artifact.id, "a1");
        assert_eq!(
            reached.load(Ordering::SeqCst),
            0,
            "nothing of the artifact index asks Weaviate once it is promoted"
        );

        rollback_artifacts(&config, &quiet).await.unwrap();
        assert!(!vellev_artifacts(&config).unwrap().promoted().await.unwrap());
        // After a rollback, a hub restarting on vellev is let through only
        // while no shadow is kept again; a shadow hub's lease says one is.
        guard_artifacts(&primary).await.unwrap();
        conn.batch_execute("SELECT artifact_mirror_renew('90 seconds')")
            .await
            .unwrap();
        assert!(guard_artifacts(&primary).await.is_err());
    })
    .await;
}

//! The vector backend's health read from how this process's knowledge
//! services last saw their repos: in a test binary of its own, since every
//! service a process makes is kept for its reports.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use workforce_codebase_knowledge::graph::vectors::{FillStage, VectorFill};
use workforce_codebase_knowledge::lease::IndexPhase;
use workforce_codebase_knowledge::testing::Held;
use workforce_codebase_knowledge::vector::CodeVectorStore;
use workforce_common::settings::{CodegraphConfig, KnowledgeConfig, KnowledgeSummariserConfig};
use workforce_hub::knowledge::{
    create_service, BackendHealthProbe, HubSummariserBuilder, VectorBackend, VectorBackendHealth,
    VectorUpkeep,
};

/// Of three repos, one complete and one being adopted: the probe says the
/// store is adopting, one of three complete. A store that keeps no track
/// of its repos is healthy, whatever they are.
#[tokio::test]
async fn a_probe_made_with_new_reads_the_adopting_repos() {
    let dir = tempfile::tempdir().unwrap();
    let held = Arc::new(Held::default());
    let store = Arc::clone(&held) as Arc<dyn CodeVectorStore>;
    let config = KnowledgeConfig {
        vector_backfill_per_sec: 0,
        ..KnowledgeConfig::default()
    };
    let service = create_service(
        &config,
        VectorBackend::Given(Arc::clone(&store)),
        dir.path().to_str().unwrap(),
        &CodegraphConfig::default(),
        None,
        HubSummariserBuilder::new(KnowledgeSummariserConfig::default()),
        VectorUpkeep::Left,
    )
    .unwrap();
    let graph = service.graph();
    let collection = store.identity(true).await.unwrap();
    for repo in ["complete", "adopting", "filling"] {
        graph.register_repo(repo, repo, "/nowhere").await.unwrap();
        graph
            .update_index_state_full(repo, "c", 0, 0)
            .await
            .unwrap();
    }
    graph
        .set_vectors_complete_in("complete", Some(&collection))
        .await
        .unwrap();
    let adopting = VectorFill {
        generation: "g".into(),
        collection,
        stage: FillStage::AdoptEntities,
        cursor: String::new(),
        done: 0,
        total: 10,
    };
    assert!(graph
        .advance_vector_fill("adopting", None, &adopting)
        .await
        .unwrap());
    // Being indexed, each is left as it is by the pass that looks at it.
    for repo in ["adopting", "filling"] {
        graph
            .acquire_lease(repo, "another-hub", None, IndexPhase::Parsing)
            .await
            .unwrap();
    }
    service.backfill_vectors().await.unwrap();

    let probe = BackendHealthProbe::new(Some(Arc::clone(&store)));
    let found = probe.current().await;
    assert!(
        matches!(
            found,
            VectorBackendHealth::Adopting {
                complete: 1,
                repos: 3,
                ..
            }
        ),
        "{found:?}"
    );
    held.untracked.store(true, Ordering::SeqCst);
    let found = probe.current().await;
    assert!(
        matches!(found, VectorBackendHealth::Healthy { .. }),
        "{found:?}"
    );
}

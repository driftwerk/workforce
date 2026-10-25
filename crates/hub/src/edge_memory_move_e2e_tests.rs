//! The edge's memories moved out of the hub's stores end to end (DW-676
//! (b)): a real Weaviate (`WorkforceMemory` and the hub's embedded class),
//! a real vellev node (the hub's replica and the edge's own tenant, which
//! embeds with the node's model) and Postgres (the replica's lock).
//!
//! Needs `WORKFORCE_TEST_WEAVIATE_URL` and a node that runs the local
//! model, as the shadow end-to-end test does.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::json;
use workforce_common::settings::{EmbeddingsProvider, WorkforceConfig, SHADOWED};
use workforce_common::telemetry_postgres::PostgresTelemetryStore;
use workforce_core::types::AgentId;
use workforce_memory::admin::{MemoryAdmin, SyncSource};
use workforce_memory::edge_owner::{external_owner, LESSON_IDENTITY};
use workforce_memory::store::MemoryStore;
use workforce_memory::vellev::{VellevStore, DEFAULT_COLLECTION};

use super::*;
use crate::memory_erasure::e2e_tests::{embedder, weaviate};

fn object(id: uuid::Uuid, memory: &str, owner: &str, identity: &str) -> Value {
    json!({
        "id": id.to_string(),
        "properties": {
            "memoryId": memory,
            "owner": owner,
            "identityId": identity,
            "content": format!("otters remember {memory}"),
            "layer": "semantic",
            "source": "explicit",
            "visibility": "private",
            "importance": 0.5,
            "createdAt": "2026-01-01T00:00:00Z",
            "accessCount": 0,
            "deleted": false,
            "tags": [],
        }
    })
}

async fn memories_in(store: &dyn SyncSource, ids: &[&str]) -> Vec<String> {
    let mut held = Vec::new();
    for id in ids {
        if !store.objects_of_memory(id, false).await.unwrap().is_empty() {
            held.push(id.to_string());
        }
    }
    held
}

#[tokio::test(flavor = "multi_thread")]
async fn the_edge_s_memories_move_to_its_tenant_and_leave_every_hub_store() {
    let Some(weaviate_url) = weaviate() else {
        return;
    };
    let Some(hub_node) = workforce_vellev::testing::Node::fresh().await else {
        return;
    };
    let Some(edge_node) = workforce_vellev::testing::Node::embedding().await else {
        return;
    };
    let ran = Arc::new(AtomicUsize::new(0));
    let reached = Arc::clone(&ran);
    let pg: Arc<std::sync::Mutex<Option<workforce_common::postgres::PostgresConfig>>> =
        Arc::default();
    let seen = Arc::clone(&pg);
    workforce_postgres::test_support::with_test_pool(
        move |cfg| {
            *seen.lock().unwrap() = Some(cfg.clone());
            PostgresTelemetryStore::build_pool(cfg)
        },
        |_pool| async move {
            let (embed_url, _) = embedder().await;
            let mut config = WorkforceConfig::default();
            config.memory.backend = SHADOWED.into();
            config.weaviate.enabled = true;
            config.weaviate.url = weaviate_url.clone();
            config.vellev.url = hub_node.url.clone();
            config.vellev.api_key = Some(hub_node.key.clone());
            config.vellev.tenant = hub_node.tenant.clone();
            config.embeddings.provider = EmbeddingsProvider::Tei;
            config.embeddings.url = Some(embed_url);
            config.embeddings.model = Some("words".into());
            config.embeddings.collection =
                Some(format!("EdgeMoveE2e{}", uuid::Uuid::new_v4().simple()));
            config.postgres = pg.lock().unwrap().clone();
            // The precondition of the move: no carry-back into WorkforceMemory.
            config.embeddings.keep_previous = false;
            config.hub.data_dir = "/nonexistent-edge-move-e2e".into();

            // The shadow's tables, which hold the replica's lock, as the
            // hub has made them.
            crate::shadow::shared_store(&config).await.unwrap();

            // Agents and memories of this run alone: WorkforceMemory is one
            // class for every test against this Weaviate.
            let run = uuid::Uuid::new_v4().simple().to_string();
            let edge_agent = format!("edge-{run}");
            let hub_agent = format!("hub-{run}");
            let memory = |name: &str| format!("{name}-{run}");
            let (m1, m2, m3, h1) = (memory("m1"), memory("m2"), memory("m3"), memory("h1"));
            let c1 = external_owner("https://accounts.google.com", &format!("c1-{run}"));
            let c2 = external_owner("https://accounts.google.com", &format!("c2-{run}"));
            let id = |_: &str| uuid::Uuid::new_v4();
            let (o1, o2, o3, o4, o5) = (id("o1"), id("o2"), id("o3"), id("o4"), id("o5"));
            // Customer 3 was erased on the edge, and the carry-back put
            // their memory back into WorkforceMemory.
            let m4 = memory("m4");
            let c3 = external_owner("https://accounts.google.com", &format!("c3-{run}"));
            let known: BTreeSet<String> = [c1.clone()].into_iter().collect();

            // WorkforceMemory: customer 1, the edge's lesson, the hub's own.
            // Weaviate's vectorizer is a stand-in that only answers ready, so
            // these bring their vectors.
            let v1 = WeaviateStore::new(&weaviate_url, None);
            // Another test may make the class at the same moment; the
            // second look finds it made.
            if v1.ensure_collection().await.is_err() {
                v1.ensure_collection().await.unwrap();
            }
            let with_vector = |mut o: Value| {
                o["vector"] = json!([0.1, 0.2, 0.3, 0.4]);
                o
            };
            v1.import(&[
                with_vector(object(o1, &m1, &edge_agent, &c1)),
                with_vector(object(o3, &m3, &edge_agent, LESSON_IDENTITY)),
                with_vector(object(o4, &h1, &hub_agent, "user-1")),
                with_vector(object(o5, &m4, &edge_agent, &c3)),
            ])
            .await
            .unwrap();
            // The hub's copies: customer 2 was erased on the edge, in
            // WorkforceMemory alone, so survives here.
            let (replica, embedded) = crate::embeddings::shadow_stores(&config).await.unwrap();
            let copies = [
                object(o1, &m1, &edge_agent, &c1),
                object(o2, &m2, &edge_agent, &c2),
                object(o4, &h1, &hub_agent, "user-1"),
            ];
            for store in [&replica, &embedded] {
                store.ensure_collection().await.unwrap();
                assert_eq!(store.upsert(&copies).await.unwrap(), 3);
            }

            let edge = EdgeTenant {
                url: edge_node.url.clone(),
                api_key: edge_node.key.clone(),
                tenant: edge_node.tenant.clone(),
                model: workforce_vellev::LOCAL_MODEL.into(),
                dimensions: workforce_vellev::LOCAL_DIMENSIONS,
                query_prefix: Some(workforce_vellev::LOCAL_QUERY_PREFIX.into()),
            };
            let agents = vec![edge_agent.clone()];
            let stores = open(&config, &edge, &agents).await.unwrap();
            let edge_store =
                VellevStore::new(&edge.url, &edge.api_key, &edge.tenant, DEFAULT_COLLECTION)
                    .unwrap()
                    .with_local_model(&edge.model, edge.dimensions, edge.query_prefix.as_deref());
            let all = [
                m1.as_str(),
                m2.as_str(),
                m3.as_str(),
                m4.as_str(),
                h1.as_str(),
            ];

            // A purge before a copy is refused; a dry run changes nothing.
            let refused = purge(&stores, &agents, true).await.unwrap_err();
            assert!(
                format!("{refused:#}").contains("copy step first"),
                "{refused:#}"
            );
            let dry = copy(&stores, &agents, &known, false).await.unwrap();
            assert_eq!(dry.survey.to_copy.len(), 2);
            assert!(memories_in(&edge_store, &all).await.is_empty());

            let copied = copy(&stores, &agents, &known, true).await.unwrap();
            assert_eq!(copied.sent, 2);
            assert_eq!(
                memories_in(&edge_store, &all).await,
                [m1.clone(), m3.clone()]
            );
            let again = copy(&stores, &agents, &known, true).await.unwrap_err();
            assert!(format!("{again:#}").contains("already"), "{again:#}");

            // The edge, now on its tenant, recalls its customer's memory
            // there, and erases the customer there.
            let recalled = edge_store
                .recall(
                    "what do otters remember",
                    workforce_memory::types::MemoryFilter {
                        owner: Some(AgentId::new(&edge_agent)),
                        identity_id: Some(c1.clone()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                recalled.first().map(|hit| hit.memory.id.0.as_str()),
                Some(m1.as_str())
            );
            // The edge's score_map puts the node's certainty on the scale
            // its floors were tuned on: every recall of the node's model
            // is mapped, here to a flat 0.99.
            let mapped =
                VellevStore::new(&edge.url, &edge.api_key, &edge.tenant, DEFAULT_COLLECTION)
                    .unwrap()
                    .with_local_model(&edge.model, edge.dimensions, edge.query_prefix.as_deref())
                    .with_score_map(vec![(0.0, 0.99), (1.0, 0.99)]);
            let scoped = |floor: f32| workforce_memory::types::MemoryFilter {
                owner: Some(AgentId::new(&edge_agent)),
                identity_id: Some(c1.clone()),
                min_certainty: Some(floor),
                ..Default::default()
            };
            let hits = mapped
                .recall("what do otters remember", scoped(0.98))
                .await
                .unwrap();
            assert_eq!(hits.len(), 1);
            assert!((hits[0].score - 0.99).abs() < 1e-6, "{}", hits[0].score);
            assert!(mapped
                .recall("what do otters remember", scoped(0.995))
                .await
                .unwrap()
                .is_empty());

            assert_eq!(
                edge_store
                    .purge_identity(&AgentId::new(&edge_agent), &c1)
                    .await
                    .unwrap(),
                1
            );

            let purged = purge(&stores, &agents, true).await.unwrap();
            assert_eq!(purged.purged, 4);
            for (name, store) in [
                ("WorkforceMemory", &v1 as &dyn SyncSource),
                ("embedded", embedded.as_ref()),
                ("replica", replica.as_ref()),
            ] {
                assert_eq!(
                    memories_in(store, &all).await,
                    std::slice::from_ref(&h1),
                    "{name} keeps the hub's memory alone"
                );
            }
            assert_eq!(
                memories_in(&edge_store, &all).await,
                std::slice::from_ref(&m3),
                "the erased customer is not brought back"
            );
            // Run again, it finds nothing.
            let quiet = purge(&stores, &agents, false).await.unwrap();
            assert!(quiet.survey.to_purge.is_empty());

            embedded.delete_collection().await.unwrap();
            replica.delete_collection().await.unwrap();
            v1.purge_memories(&[workforce_memory::types::MemoryId(h1.clone())])
                .await
                .unwrap();
            reached.fetch_add(1, Ordering::SeqCst);
        },
    )
    .await;
    assert!(
        ran.load(Ordering::SeqCst) == 1 || std::env::var("WORKFORCE_SKIP_POSTGRES_TESTS").is_ok()
    );
}

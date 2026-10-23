//! DW-526 — concurrency isolation harness.
//!
//! Spawns N concurrent recall tasks under distinct `CURRENT_USER_ID`
//! scopes against a shared `MemoryStore`, then asserts each task only sees
//! its own user's memories. Regressions of the kind DW-518 closed
//! (fail-open recall when the task-local is unset, identity propagation
//! lost across spawned tasks) re-surface as cross-task leaks here.

use std::sync::Arc;

use workforce_agent::runner::CURRENT_USER_ID;
use workforce_core::types::AgentId;
use workforce_memory::in_memory::InMemoryStore;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{Memory, MemoryFilter, MemoryLayer};

const USERS: &[&str] = &[
    "alice", "bob", "carol", "dave", "erin", "frank", "gina", "henry",
];

async fn seed_store(store: &Arc<dyn MemoryStore>) {
    let agent = AgentId::new("atlas");
    for user in USERS {
        for i in 0..3 {
            let m = Memory::new(
                format!("{user}-secret-{i}"),
                MemoryLayer::Episodic,
                agent.clone(),
            )
            .with_identity_id(*user);
            store.remember(m).await.unwrap();
        }
    }
}

async fn recall_for_current_user(store: Arc<dyn MemoryStore>) -> Vec<String> {
    let identity_id = CURRENT_USER_ID.try_with(|u| u.clone()).ok();
    let filter = MemoryFilter {
        identity_id,
        limit: Some(50),
        ..Default::default()
    };
    store
        .recall("secret", filter)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.memory.content)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_recalls_never_leak_across_users() {
    let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
    seed_store(&store).await;

    let mut handles = Vec::with_capacity(USERS.len() * 4);
    for round in 0..4 {
        for user in USERS {
            let user = user.to_string();
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                let results = CURRENT_USER_ID
                    .scope(user.clone(), recall_for_current_user(store))
                    .await;
                (round, user, results)
            }));
        }
    }

    for handle in handles {
        let (round, user, results) = handle.await.expect("task panicked");
        assert_eq!(
            results.len(),
            3,
            "round {round} user {user}: expected 3 own memories, got {results:?}"
        );
        for content in &results {
            assert!(
                content.starts_with(&format!("{user}-")),
                "round {round} user {user} saw foreign memory: {content}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_recalls_with_no_identity_return_nothing() {
    let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
    seed_store(&store).await;

    let handles: Vec<_> = (0..16)
        .map(|_| {
            let store = Arc::clone(&store);
            tokio::spawn(async move { recall_for_current_user(store).await })
        })
        .collect();

    for handle in handles {
        let results = handle.await.unwrap();
        assert!(
            results.is_empty(),
            "fail-closed: unscoped recall must return nothing, got {results:?}"
        );
    }
}

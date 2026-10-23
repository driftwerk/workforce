//! Phase-8 concurrency test for the cascade evaluator. Mirrors DW-526's
//! memory-isolation harness: spawn N concurrent evaluations of distinct
//! `(user, agent, tool)` triples against a single shared store, then
//! assert each task gets back the decision the cascade would have
//! computed if it were the only caller. Regressions of "shared cache
//! returns one user's decision to another user" (the kind of bug the
//! generation counter is supposed to prevent) re-surface here.

use std::sync::Arc;

use workforce_permissions::cache::PermissionEvaluator;
use workforce_permissions::cascade::CascadeRequest;
use workforce_permissions::sqlite::SqlitePermissionStore;
use workforce_permissions::store::PermissionStore;
use workforce_permissions::types::{
    PermissionOrigin, PermissionRule, PermissionScope, PermissionState,
};

const USERS: &[&str] = &[
    "alice", "bob", "carol", "dave", "erin", "frank", "gina", "henry",
];

fn seed(store: &Arc<dyn PermissionStore>) {
    // For each user we plant a single User-scope row pinning
    // `memory.recall` to a distinct state. The cascade should return
    // that state for the user-owned triple, and `Allow` (default) for
    // any other user looking at the same tool. The point is that
    // mixing them concurrently must never cause one user's decision
    // to leak into another's evaluation.
    let states = [
        PermissionState::Allow,
        PermissionState::Ask,
        PermissionState::AlwaysAsk,
        PermissionState::Deny,
        PermissionState::Allow,
        PermissionState::Ask,
        PermissionState::AlwaysAsk,
        PermissionState::Deny,
    ];
    for (user, state) in USERS.iter().zip(states.iter()) {
        let rule = PermissionRule::new(
            PermissionScope::User,
            *user,
            "memory.recall",
            *state,
            PermissionOrigin::UserRuntimeChoice,
        );
        store.upsert_rule(&rule).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_evaluators_never_swap_users_decisions() {
    let store: Arc<dyn PermissionStore> = Arc::new(SqlitePermissionStore::in_memory().unwrap());
    seed(&store);
    let evaluator = Arc::new(PermissionEvaluator::new(Arc::clone(&store)));

    let expected: Vec<(&str, PermissionState)> = USERS
        .iter()
        .zip(
            [
                PermissionState::Allow,
                PermissionState::Ask,
                PermissionState::AlwaysAsk,
                PermissionState::Deny,
                PermissionState::Allow,
                PermissionState::Ask,
                PermissionState::AlwaysAsk,
                PermissionState::Deny,
            ]
            .iter()
            .copied(),
        )
        .map(|(u, s)| (*u, s))
        .collect();

    let mut handles = Vec::with_capacity(USERS.len() * 8);
    for round in 0..8 {
        for &(user, expected_state) in &expected {
            let evaluator = Arc::clone(&evaluator);
            let user = user.to_string();
            handles.push(tokio::spawn(async move {
                let teams: Vec<String> = Vec::new();
                let request = CascadeRequest {
                    user_id: &user,
                    agent_id: "atlas",
                    team_ids: &teams,
                    tool_name: "memory.recall",
                };
                let got = evaluator.evaluate(&request).unwrap();
                (round, user, expected_state, got)
            }));
        }
    }

    for h in handles {
        let (round, user, expected_state, got) = h.await.expect("task panicked");
        assert_eq!(
            got, expected_state,
            "round {round} user {user}: expected {expected_state:?}, got {got:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_during_concurrent_reads_invalidates_cache() {
    // While one task tightens a Team-scope rule for `memory.*`, the
    // remaining evaluators reading the same triple must observe the
    // new state on their next call. The DB generation counter is the
    // multi-node-safe mechanism that makes this hold.
    let store: Arc<dyn PermissionStore> = Arc::new(SqlitePermissionStore::in_memory().unwrap());
    let team = "team-a";
    store
        .upsert_rule(&PermissionRule::new(
            PermissionScope::Team,
            team,
            "memory.*",
            PermissionState::Ask,
            PermissionOrigin::Admin,
        ))
        .unwrap();
    let evaluator = Arc::new(PermissionEvaluator::new(Arc::clone(&store)));
    let teams = vec![team.to_string()];

    // First pass: every reader sees Ask.
    let mut handles = Vec::new();
    for _ in 0..32 {
        let evaluator = Arc::clone(&evaluator);
        let teams = teams.clone();
        handles.push(tokio::spawn(async move {
            let request = CascadeRequest {
                user_id: "u",
                agent_id: "a",
                team_ids: &teams,
                tool_name: "memory.recall",
            };
            evaluator.evaluate(&request).unwrap()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), PermissionState::Ask);
    }

    // Tighten the rule from a "different node" — a different evaluator
    // handle pointing at the same store. The generation counter bump
    // must propagate to the original evaluator's cache.
    store
        .upsert_rule(&PermissionRule::new(
            PermissionScope::Team,
            team,
            "memory.*",
            PermissionState::AlwaysAsk,
            PermissionOrigin::Admin,
        ))
        .unwrap();

    // Second pass: every reader (including ones that saw Ask above)
    // observes AlwaysAsk on their next call.
    let mut handles = Vec::new();
    for _ in 0..32 {
        let evaluator = Arc::clone(&evaluator);
        let teams = teams.clone();
        handles.push(tokio::spawn(async move {
            let request = CascadeRequest {
                user_id: "u",
                agent_id: "a",
                team_ids: &teams,
                tool_name: "memory.recall",
            };
            evaluator.evaluate(&request).unwrap()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), PermissionState::AlwaysAsk);
    }
}

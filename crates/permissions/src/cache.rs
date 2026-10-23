//! Per-process LRU cache for cascade decisions, made multi-node-safe by
//! comparing each cached entry's generation against the store's current
//! generation counter on every hit.
//!
//! Why a counter (DB-backed) and not pub/sub: when node A writes a
//! permission rule, node B's in-process cache has no idea unless we
//! tell it. The two options are broadcast invalidation (NATS) or pull
//! invalidation (counter check). A single bumping integer in the DB,
//! checked on every cache hit, is simpler than wiring pub/sub and
//! costs one indexed lookup. The lookup is fast enough at p99 that the
//! plan flagged this approach explicitly.

use std::num::NonZeroUsize;
use std::sync::Arc;

use eyre::Result;
use lru::LruCache;
use parking_lot::Mutex;

use crate::cascade::{self, CascadeRequest, SystemRule};
use crate::store::PermissionStore;
use crate::types::{PermissionRule, PermissionScope, PermissionState};

/// Default LRU capacity. Tuned to comfortably hold the working set of a
/// busy hub (50 users × 5 agents × 50 distinct tools = 12.5k entries)
/// without ever growing without bound.
pub const DEFAULT_CACHE_CAPACITY: usize = 16_384;

/// Tuple used as the cache key. Owned strings so the cache lives
/// independently of any request lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    user_id: String,
    agent_id: String,
    tool_name: String,
}

#[derive(Debug, Clone, Copy)]
struct CacheEntry {
    state: PermissionState,
    /// The store generation that was current when this entry was
    /// computed. If the store's counter has moved on since, the entry
    /// is stale.
    generation: u64,
}

/// Cascade decision cache. Cheap to clone (it's an `Arc<Mutex<...>>`).
#[derive(Clone)]
pub struct PermissionCache {
    inner: Arc<Mutex<LruCache<CacheKey, CacheEntry>>>,
}

impl PermissionCache {
    pub fn new(capacity: usize) -> Self {
        let capacity = NonZeroUsize::new(capacity.max(1)).expect("capacity >= 1");
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(capacity))),
        }
    }

    /// Look up a cached entry that's still consistent with
    /// `current_generation`. Returns `None` when the entry is missing
    /// or stale; the caller then recomputes and writes back via
    /// [`Self::insert`].
    fn lookup(&self, key: &CacheKey, current_generation: u64) -> Option<PermissionState> {
        let mut cache = self.inner.lock();
        let entry = cache.get(key)?;
        if entry.generation == current_generation {
            Some(entry.state)
        } else {
            None
        }
    }

    fn insert(&self, key: CacheKey, state: PermissionState, generation: u64) {
        self.inner.lock().put(key, CacheEntry { state, generation });
    }

    /// Test-only inspection: returns the number of entries currently
    /// held. Useful for asserting eviction or cache-hit behaviour.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().len()
    }
}

impl Default for PermissionCache {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_CAPACITY)
    }
}

/// Hub-facing entry point: resolves a `(user, agent, tool)` triple
/// against the System rules + DB-backed store, caching the result and
/// invalidating it via the store's generation counter.
pub struct PermissionEvaluator {
    store: Arc<dyn PermissionStore>,
    cache: PermissionCache,
    system_rules: Arc<Vec<SystemRule>>,
}

impl PermissionEvaluator {
    pub fn new(store: Arc<dyn PermissionStore>) -> Self {
        Self {
            store,
            cache: PermissionCache::default(),
            system_rules: Arc::new(Vec::new()),
        }
    }

    pub fn with_system_rules(mut self, rules: Vec<SystemRule>) -> Self {
        self.system_rules = Arc::new(rules);
        self
    }

    pub fn with_cache(mut self, cache: PermissionCache) -> Self {
        self.cache = cache;
        self
    }

    /// Get the underlying store for callers that need to write
    /// User-scope rules (the Always-* button) or read individual
    /// rules for the admin UI.
    pub fn store(&self) -> &Arc<dyn PermissionStore> {
        &self.store
    }

    /// Evaluate the cascade for one call. Cheap on cache hit
    /// (one DB lookup of the generation counter), one round-trip per
    /// scope on cache miss.
    pub fn evaluate(&self, request: &CascadeRequest<'_>) -> Result<PermissionState> {
        let generation = self.store.current_generation()?;
        let key = CacheKey {
            user_id: request.user_id.to_string(),
            agent_id: request.agent_id.to_string(),
            tool_name: request.tool_name.to_string(),
        };
        if let Some(state) = self.cache.lookup(&key, generation) {
            return Ok(state);
        }
        let db_rules = self.load_db_rules(request)?;
        let state = cascade::evaluate(request, &self.system_rules, &db_rules);
        self.cache.insert(key, state, generation);
        Ok(state)
    }

    /// Convenience for the Always-* runtime persistence path: writes a
    /// User-scope rule for the requesting user and invalidates the
    /// cache implicitly via the generation bump inside the store.
    pub fn persist_user_choice(
        &self,
        user_id: &str,
        tool_pattern: &str,
        state: PermissionState,
        created_by: Option<uuid::Uuid>,
    ) -> Result<()> {
        let mut rule = PermissionRule::new(
            PermissionScope::User,
            user_id,
            tool_pattern,
            state,
            crate::types::PermissionOrigin::UserRuntimeChoice,
        );
        if let Some(uid) = created_by {
            rule = rule.with_created_by(uid);
        }
        self.store.upsert_rule(&rule)
    }

    fn load_db_rules(&self, request: &CascadeRequest<'_>) -> Result<Vec<PermissionRule>> {
        let mut scopes: Vec<(PermissionScope, &str)> =
            Vec::with_capacity(request.team_ids.len() + 2);
        for team in request.team_ids {
            scopes.push((PermissionScope::Team, team.as_str()));
        }
        scopes.push((PermissionScope::Agent, request.agent_id));
        scopes.push((PermissionScope::User, request.user_id));
        self.store.rules_for_scopes(&scopes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::SqlitePermissionStore;
    use crate::types::{PermissionOrigin, PermissionScope, PermissionState};

    fn store() -> Arc<SqlitePermissionStore> {
        Arc::new(SqlitePermissionStore::in_memory().unwrap())
    }

    fn admin_team_rule(team: &str, tool: &str, state: PermissionState) -> PermissionRule {
        PermissionRule::new(
            PermissionScope::Team,
            team,
            tool,
            state,
            PermissionOrigin::Admin,
        )
    }

    #[test]
    fn evaluator_caches_repeat_evaluations() {
        let store = store();
        let team = "team-a";
        store
            .upsert_rule(&admin_team_rule(team, "memory.*", PermissionState::Ask))
            .unwrap();
        let evaluator = PermissionEvaluator::new(store.clone() as Arc<dyn PermissionStore>);
        let teams = vec![team.to_string()];
        let request = CascadeRequest {
            user_id: "u",
            agent_id: "a",
            team_ids: &teams,
            tool_name: "memory.recall",
        };

        let first = evaluator.evaluate(&request).unwrap();
        assert_eq!(first, PermissionState::Ask);
        assert_eq!(evaluator.cache.len(), 1);

        // A second call with the same request must hit the cache —
        // not produce a new entry.
        let second = evaluator.evaluate(&request).unwrap();
        assert_eq!(second, PermissionState::Ask);
        assert_eq!(evaluator.cache.len(), 1);
    }

    #[test]
    fn evaluator_invalidates_on_store_write_from_another_handle() {
        // Simulate the multi-node case: node A and node B share the
        // same DB but each has its own evaluator. A write through
        // either handle bumps the generation, so the other handle's
        // cached entry goes stale on its next lookup.
        let store: Arc<dyn PermissionStore> = store();
        let team = "team-a";
        store
            .upsert_rule(&admin_team_rule(team, "memory.*", PermissionState::Ask))
            .unwrap();

        let node_a = PermissionEvaluator::new(store.clone());
        let node_b = PermissionEvaluator::new(store.clone());
        let teams = vec![team.to_string()];
        let request = CascadeRequest {
            user_id: "u",
            agent_id: "a",
            team_ids: &teams,
            tool_name: "memory.recall",
        };

        assert_eq!(node_a.evaluate(&request).unwrap(), PermissionState::Ask);
        assert_eq!(node_b.evaluate(&request).unwrap(), PermissionState::Ask);

        // Tighten the rule on node A.
        store
            .upsert_rule(&admin_team_rule(
                team,
                "memory.*",
                PermissionState::AlwaysAsk,
            ))
            .unwrap();

        // Both evaluators must reflect the new state on their next call.
        assert_eq!(
            node_a.evaluate(&request).unwrap(),
            PermissionState::AlwaysAsk
        );
        assert_eq!(
            node_b.evaluate(&request).unwrap(),
            PermissionState::AlwaysAsk
        );
    }

    #[test]
    fn persist_user_choice_writes_user_scope_row_and_invalidates_cache() {
        // The runtime "Always Allow" path: user clicks the button on a
        // tool gated at Team=Ask, evaluator persists a User=Allow row,
        // next evaluate reflects the override.
        let store: Arc<dyn PermissionStore> = store();
        let team = "team-a";
        store
            .upsert_rule(&admin_team_rule(team, "memory.*", PermissionState::Ask))
            .unwrap();
        let evaluator = PermissionEvaluator::new(store.clone());
        let teams = vec![team.to_string()];
        let request = CascadeRequest {
            user_id: "u",
            agent_id: "a",
            team_ids: &teams,
            tool_name: "memory.recall",
        };

        assert_eq!(evaluator.evaluate(&request).unwrap(), PermissionState::Ask);

        evaluator
            .persist_user_choice("u", "memory.recall", PermissionState::Allow, None)
            .unwrap();

        assert_eq!(
            evaluator.evaluate(&request).unwrap(),
            PermissionState::Allow
        );
    }

    #[test]
    fn system_rules_compose_into_admin_chain() {
        let store: Arc<dyn PermissionStore> = store();
        let evaluator = PermissionEvaluator::new(store).with_system_rules(vec![SystemRule {
            tool_pattern: "vault.delete".to_string(),
            state: PermissionState::Deny,
        }]);
        let request = CascadeRequest {
            user_id: "u",
            agent_id: "a",
            team_ids: &[],
            tool_name: "vault.delete",
        };
        assert_eq!(evaluator.evaluate(&request).unwrap(), PermissionState::Deny);
    }

    #[test]
    fn unrelated_writes_still_bump_cache_generation() {
        // Property check on the generation counter: any write bumps,
        // even a write that has nothing to do with the cached entry.
        // That's a conservative invalidation but it's also the
        // simplest correct one — and writes are infrequent enough
        // that the over-invalidation doesn't matter.
        let store: Arc<dyn PermissionStore> = store();
        let team = "team-a";
        store
            .upsert_rule(&admin_team_rule(team, "memory.*", PermissionState::Ask))
            .unwrap();
        let evaluator = PermissionEvaluator::new(store.clone());
        let teams = vec![team.to_string()];
        let request = CascadeRequest {
            user_id: "u",
            agent_id: "a",
            team_ids: &teams,
            tool_name: "memory.recall",
        };

        let _ = evaluator.evaluate(&request).unwrap();
        let before = store.current_generation().unwrap();

        // Write an unrelated rule.
        store
            .upsert_rule(&admin_team_rule(
                "team-b",
                "vault.*",
                PermissionState::AlwaysAsk,
            ))
            .unwrap();

        let after = store.current_generation().unwrap();
        assert!(after > before);

        // The cached entry is now considered stale (generation
        // mismatch) — the evaluator will recompute. Result is the
        // same `Ask` (no rule changed for this triple), but the
        // re-compute is observable: we record the new generation.
        assert_eq!(evaluator.evaluate(&request).unwrap(), PermissionState::Ask);
    }
}

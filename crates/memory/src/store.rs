use std::collections::BTreeMap;

use async_trait::async_trait;
use eyre::Result;

use workforce_core::types::AgentId;

use crate::access_audit::{self, AccessAuditEvent, AuditDecision, AuditOperation};
use crate::types::{Memory, MemoryFilter, MemoryId, MemorySearchResult, UserId, Visibility};
use crate::visibility::{AccessController, AccessSubject};

/// Default minimum cosine similarity (= Weaviate `certainty`) for two pieces
/// of memory content to be considered near-duplicates.
///
/// Tight by design: 0.92 collapses paraphrases of the same situation
/// ("user asked X, I couldn't run it") but keeps legitimate recurrences of a
/// topic separated by genuinely different context. Tuned conservatively
/// because false-positive merges silently lose information, while false
/// negatives only mean we keep one extra memory.
pub const DEFAULT_NEAR_DUPLICATE_SIMILARITY: f32 = 0.92;

/// Minimum importance value enforced by [`MemoryStore::adjust_importance`]
/// regardless of how many negative-reinforcement signals fire. Never zero so
/// long-demoted lessons remain audit-visible.
pub const IMPORTANCE_FLOOR: f32 = 0.05;

/// Returns `true` when none of the memory's tags appear in `excluded`.
///
/// Used by every backend's `recall` / `list` as a uniform post-filter: the
/// underlying query may not natively support array-tag negation (Weaviate's
/// `text[]` semantics make it awkward; SQLite would need a clumsy NOT LIKE
/// chain), so we fetch and drop in-process. Empty `excluded` is a no-op.
pub fn passes_excluded_tags(memory: &Memory, excluded: &[String]) -> bool {
    excluded.is_empty() || !memory.tags.iter().any(|t| excluded.iter().any(|e| e == t))
}

/// Positive tag filter with the OR semantics shared by SQLite/InMemory: a
/// non-empty `required` list admits only memories carrying at least one of
/// the tags as an EXACT string. Backends whose native tag matching is
/// token-based must call this after decode — Weaviate word-tokenizes the
/// `tags` property on legacy collections, so `date:2026-07-05` cross-matches
/// `date:2026-05-07` (identical token sets) and months-old memories leak
/// into the wrong day's context. `None`/empty is a no-op.
pub fn passes_required_tags(memory: &Memory, required: Option<&[String]>) -> bool {
    match required {
        Some(tags) if !tags.is_empty() => memory.tags.iter().any(|t| tags.iter().any(|r| r == t)),
        _ => true,
    }
}

/// Append audit-pollution kind tags to `filter.excluded_tags` when the
/// caller has not opted in to forensic access via `include_polluted`.
///
/// Called by every backend at the top of `recall` and `list` so that
/// pollution exclusion is the default behaviour and bypassing it requires
/// an explicit, named flag rather than remembering to set `excluded_tags`.
/// Idempotent: running it twice (or against a filter that already lists
/// the kind tags) does not duplicate them.
pub fn apply_pollution_default(filter: &mut MemoryFilter) {
    if filter.include_polluted {
        return;
    }
    for kind in crate::audit::POLLUTED_KIND_TAGS {
        let kind_owned = kind.to_string();
        if !filter.excluded_tags.iter().any(|t| t == &kind_owned) {
            filter.excluded_tags.push(kind_owned);
        }
    }
}

/// Per-user isolation gate (DW-518): every backend's `recall`, `list`,
/// and `count` calls this and returns empty / zero when it returns
/// `false`. A filter that neither names an `identity_id` nor explicitly
/// opts in via `system_unscoped` is rejected — preventing cross-user
/// reads when an entry path forgets to propagate `CURRENT_USER_ID`.
pub fn is_identity_scope_authorised(filter: &MemoryFilter) -> bool {
    filter.identity_id.is_some() || filter.system_unscoped
}

/// Defence-in-depth post-filter that drops memories the backend somehow
/// returned despite the `MemoryFilter` saying otherwise (DW-525). Wraps
/// the user-centric [`AccessController`] so the access decision lives in
/// one place rather than being re-implemented in every backend.
///
/// Emits a `DeniedOwnerMismatch` audit event (DW-527) for every dropped
/// row. `system_unscoped` filters bypass this — the call is by design
/// unscoped (consolidation, audit tooling) and the per-user check is
/// meaningless.
pub fn enforce_access_control<I, M>(
    filter: &MemoryFilter,
    operation: AuditOperation,
    results: I,
) -> Vec<M>
where
    I: IntoIterator<Item = M>,
    M: AsMemory,
{
    let Some(uid) = filter
        .identity_id
        .as_deref()
        .filter(|_| !filter.system_unscoped)
    else {
        return results.into_iter().collect();
    };
    let user = UserId::from(uid);
    let mut subject = AccessSubject::for_user(&user);
    if let Some(team) = filter.team_id.as_deref() {
        subject = subject.with_team(team);
    }
    let controller = AccessController::new();
    results
        .into_iter()
        .filter_map(|m| {
            let mem = m.as_memory();
            if controller.check_read(mem, &subject).is_allow() {
                Some(m)
            } else {
                access_audit::emit(AccessAuditEvent::new(
                    operation,
                    AuditDecision::DeniedOwnerMismatch,
                    Some(uid),
                    mem.identity_id.as_deref(),
                    Some(mem.id.0.as_str()),
                ));
                None
            }
        })
        .collect()
}

/// Narrow recall results to the memories an agent may surface under the
/// `Shared` recall scope: those it owns, plus any shared beyond their owner
/// (non-`Private` visibility). This is intra-user narrowing — cross-user
/// access is already enforced upstream by the filter's `identity_id` and
/// [`enforce_access_control`]. The stricter `Isolated` scope is applied at
/// the filter level (`owner`), not here.
pub fn retain_shared_recallable<M: AsMemory>(
    agent: &AgentId,
    results: impl IntoIterator<Item = M>,
) -> Vec<M> {
    results
        .into_iter()
        .filter(|m| {
            let mem = m.as_memory();
            mem.owner == *agent || !matches!(mem.visibility, Visibility::Private)
        })
        .collect()
}

/// The one post-filter every agent-facing recall surface applies after its
/// backend query: `Isolated` keeps only `agent`'s own memories, `Shared`
/// applies [`retain_shared_recallable`]. Callers should ALSO push the owner
/// into the backend filter under `Isolated`
/// (`scope.owned_only().then(|| agent.clone())`) so foreign rows don't eat
/// the query's `limit` budget — this function is the correctness half, the
/// filter owner is the efficiency half.
pub fn apply_agent_scope<M: AsMemory>(
    scope: workforce_common::settings::AgentRecallScope,
    agent: &AgentId,
    results: impl IntoIterator<Item = M>,
) -> Vec<M> {
    if scope.owned_only() {
        results
            .into_iter()
            .filter(|m| m.as_memory().owner == *agent)
            .collect()
    } else {
        retain_shared_recallable(agent, results)
    }
}

/// Combined gate + audit emission used at the top of every backend's
/// `recall` / `list` / `count`. Returns `false` when the call should
/// short-circuit with an empty result; the corresponding denial event
/// has already been emitted to the audit sink.
pub fn check_and_audit_scope(filter: &MemoryFilter, operation: AuditOperation) -> bool {
    if is_identity_scope_authorised(filter) {
        access_audit::emit(AccessAuditEvent::new(
            operation,
            AuditDecision::Allow,
            filter.identity_id.as_deref(),
            None,
            None,
        ));
        return true;
    }
    access_audit::record_unscoped_denial(operation, filter);
    false
}

/// Drop later copies of a memory from `results`, keeping the first, the
/// most relevant: a store may return one memory more than once (Weaviate
/// 1.36.x does when a query straddles replicas or matches several filter
/// branches).
pub(crate) fn dedup_by_memory_id<M: AsMemory>(results: Vec<M>) -> Vec<M> {
    let mut seen = std::collections::HashSet::with_capacity(results.len());
    results
        .into_iter()
        .filter(|m| seen.insert(m.as_memory().id.0.clone()))
        .collect()
}

/// What a raw-object backend's `recall` and `list` apply to what its query
/// returned, whatever the query already filtered: each memory once, the
/// exact tag and event-time filters, access control, and at most `limit`.
pub(crate) fn post_filtered<M: AsMemory>(
    filter: &MemoryFilter,
    operation: AuditOperation,
    results: Vec<M>,
    limit: usize,
) -> Vec<M> {
    let kept = dedup_by_memory_id(results).into_iter().filter(|m| {
        let memory = m.as_memory();
        passes_excluded_tags(memory, &filter.excluded_tags)
            && passes_required_tags(memory, filter.tags.as_deref())
            && filter
                .event_after
                .is_none_or(|after| memory.event_time >= after)
            && filter
                .event_before
                .is_none_or(|before| memory.event_time <= before)
    });
    enforce_access_control(filter, operation, kept)
        .into_iter()
        .take(limit)
        .collect()
}

/// Adapter trait so `enforce_access_control` works uniformly over `Memory`
/// and `MemorySearchResult` without forcing each backend to flatten or
/// re-wrap its result type.
pub trait AsMemory {
    fn as_memory(&self) -> &Memory;
}

impl AsMemory for Memory {
    fn as_memory(&self) -> &Memory {
        self
    }
}

impl AsMemory for MemorySearchResult {
    fn as_memory(&self) -> &Memory {
        &self.memory
    }
}

/// Snapshot of a memory store's on-disk / in-memory footprint, for
/// process-level memory observability. The default implementation
/// returns zeros so backends without local state (Weaviate, etc.)
/// don't need to override.
#[derive(Debug, Default, Clone, Copy)]
pub struct MemoryStoreCacheStats {
    /// Size in bytes of the local on-disk database, when applicable.
    pub db_bytes: u64,
}

/// Trait for memory storage backends.
///
/// Implementations can use Weaviate, PostgreSQL, in-memory storage, etc.
/// How a store the hub embeds for ranks a recall ([`MemoryStore::recall_settings`]).
#[derive(Debug, Clone, PartialEq)]
pub struct RecallSettings {
    /// The hybrid's weight on the vector side (1) against keywords (0).
    pub alpha: f32,
    /// Whether the candidates are reranked.
    pub reranked: bool,
    /// How many candidates are reranked.
    pub rerank_candidates: usize,
    /// How certainties are reported.
    pub score_map: Vec<(f32, f32)>,
}

#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Snapshot the store's local footprint for memory observability.
    /// Default returns zeros — implementations with on-disk or in-process
    /// state (e.g. SQLite) override.
    fn cache_stats(&self) -> MemoryStoreCacheStats {
        MemoryStoreCacheStats::default()
    }

    /// Store a new memory. Returns the memory ID.
    async fn remember(&self, memory: Memory) -> Result<MemoryId>;

    /// Semantic search for relevant memories.
    async fn recall(&self, query: &str, filter: MemoryFilter) -> Result<Vec<MemorySearchResult>>;

    /// The model a vector given to [`Self::recall_with_vector`] must come
    /// from: `Some` only for a store the hub embeds for, which then uses
    /// that vector instead of embedding the query itself.
    fn query_model(&self) -> Option<String> {
        None
    }

    /// How this store ranks a recall when the hub embeds for it: `None`
    /// for a store that does not. Shadow mode's two stores must agree on
    /// it, or the comparison measures settings, not indexes (DW-676, A13).
    fn recall_settings(&self) -> Option<RecallSettings> {
        None
    }

    /// [`Self::recall`] with the query's vector given rather than made here
    /// (`None`: it could not be made, so by keywords alone), so that one
    /// embedding serves two stores (DW-676: shadow mode compares indexes,
    /// not embedding calls). A store that takes no vector recalls as
    /// `recall` does.
    async fn recall_with_vector(
        &self,
        query: &str,
        vector: Option<Vec<f32>>,
        filter: MemoryFilter,
    ) -> Result<Vec<MemorySearchResult>> {
        let _ = vector;
        self.recall(query, filter).await
    }

    /// Get a specific memory by ID.
    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>>;

    /// Soft-delete a memory (with audit trail).
    async fn forget(&self, id: &MemoryId) -> Result<()>;

    /// Update a memory's visibility.
    async fn share(&self, id: &MemoryId, visibility: crate::types::Visibility) -> Result<()>;

    /// Count memories matching a filter.
    async fn count(&self, filter: MemoryFilter) -> Result<usize>;

    /// List memories matching a filter without semantic search.
    async fn list(&self, filter: MemoryFilter) -> Result<Vec<Memory>>;

    /// Update the tags on a memory.
    async fn update_tags(&self, id: &MemoryId, tags: Vec<String>) -> Result<()>;

    /// Atomically apply `delta` to a memory's `importance`, clamp the result
    /// to `[IMPORTANCE_FLOOR, 1.0]`, and return the new value. Two concurrent
    /// callers — one positive, one negative — must compose to the net delta
    /// rather than the last writer's value; backends therefore implement this
    /// without a read-modify-write window (SQL `UPDATE … RETURNING`, in-process
    /// mutex). The Weaviate backend has no atomic increment primitive and is
    /// best-effort — see `WeaviateMemoryStore::adjust_importance`.
    ///
    /// Returns `Err` when the memory does not exist; callers (reinforcement,
    /// recurrence-bump) decide whether to log-and-continue or surface.
    async fn adjust_importance(&self, id: &MemoryId, delta: f32) -> Result<f32>;

    /// Every memory `owner` holds about `identity`, forgotten ones included:
    /// what erasure has to reach. Bypasses the per-user read gate, so only an
    /// identity lifecycle operation may call it.
    async fn identity_memories(&self, owner: &AgentId, identity: &str) -> Result<Vec<MemoryId>>;

    /// How many memories `owner` holds about each identity, forgotten ones
    /// included, never the empty one: who erasure of the whole agent has to
    /// reach and what an identity clean-up reads to find people who no longer
    /// exist. Fails rather than undercount. Bypasses the per-user read gate,
    /// so only an identity lifecycle operation may call it.
    async fn identity_counts(&self, owner: &AgentId) -> Result<BTreeMap<String, usize>>;

    /// Every identity in [`Self::identity_counts`], each once.
    async fn identities(&self, owner: &AgentId) -> Result<Vec<String>> {
        Ok(self.identity_counts(owner).await?.into_keys().collect())
    }

    /// Move every memory `owner` holds about `from` to `to`, forgotten ones
    /// included. Returns how many moved.
    async fn reassign_identity(&self, owner: &AgentId, from: &str, to: &str) -> Result<usize>;

    /// Delete every memory `owner` holds about `identity` outright, forgotten
    /// ones included ([`Self::forget`] only hides a memory). Returns how many
    /// were deleted.
    async fn purge_identity(&self, owner: &AgentId, identity: &str) -> Result<usize>;

    /// Delete the memories `ids` outright, wherever they are held. Returns
    /// how many were deleted.
    async fn purge_memories(&self, ids: &[MemoryId]) -> Result<usize>;

    /// Find the closest near-duplicate of `content` whose similarity meets or
    /// exceeds `min_similarity`, scoped by `filter`. Returns `None` when no
    /// candidate clears the bar.
    ///
    /// Implementations with vector search should perform a single top-1
    /// similarity query. The default falls back to `recall` and applies the
    /// threshold in-process — correct for any backend, but pays a full
    /// recall's overhead.
    ///
    /// Callers MUST scope `filter` to at least the same owner and identity;
    /// without scoping, two different users' similar topics could merge.
    async fn find_near_duplicate(
        &self,
        content: &str,
        mut filter: MemoryFilter,
        min_similarity: f32,
    ) -> Result<Option<MemorySearchResult>> {
        filter.limit = Some(1);
        let mut results = self.recall(content, filter).await?;
        let candidate = results.drain(..).next();
        Ok(candidate.filter(|r| r.score >= min_similarity))
    }

    /// Record that an existing memory has been re-extracted as a near-duplicate.
    ///
    /// Increments `recurrence_count`, updates `last_recurrence_at` to now, and
    /// appends `additional_message_ids` to `source_message_ids` (preserving
    /// order, dropping IDs already linked).
    ///
    /// Returns the new `recurrence_count`.
    async fn record_recurrence(
        &self,
        id: &MemoryId,
        additional_message_ids: Vec<String>,
    ) -> Result<u32>;
}

/// What every backend's identity operations must do alike.
#[cfg(test)]
pub(crate) async fn identity_operations_contract(store: &dyn MemoryStore) {
    use crate::types::MemoryLayer;
    let owner = AgentId::new("agent");
    let other_agent = AgentId::new("other");
    let about = |content: &str, owner: &AgentId, who: &str| {
        let mut memory = Memory::new(content, MemoryLayer::Episodic, owner.clone());
        memory.identity_id = Some(who.to_string());
        memory
    };
    let kept = store
        .remember(about("alice likes tea", &owner, "alice"))
        .await
        .unwrap();
    let forgotten = store
        .remember(about("alice moved", &owner, "alice"))
        .await
        .unwrap();
    store.forget(&forgotten).await.unwrap();
    store
        .remember(about("bob likes coffee", &owner, "bob"))
        .await
        .unwrap();
    let elsewhere = store
        .remember(about("alice elsewhere", &other_agent, "alice"))
        .await
        .unwrap();
    // The same identities, held by another agent: none of what is done
    // to the owner's touches them.
    let bob_elsewhere = store
        .remember(about("bob elsewhere", &other_agent, "bob"))
        .await
        .unwrap();
    let neighbour = store
        .remember(about("bobby likes juice", &owner, "bobby"))
        .await
        .unwrap();
    store
        .remember(about("nobody in particular", &owner, ""))
        .await
        .unwrap();

    assert_eq!(
        store.identity_counts(&owner).await.unwrap(),
        BTreeMap::from([
            ("alice".to_string(), 2),
            ("bob".to_string(), 1),
            ("bobby".to_string(), 1)
        ]),
        "forgotten memories are counted, other agents' and the empty identity are not"
    );
    assert_eq!(
        store.identities(&owner).await.unwrap(),
        ["alice", "bob", "bobby"]
    );
    assert_eq!(
        store.identities(&other_agent).await.unwrap(),
        ["alice", "bob"]
    );
    let mut held = store.identity_memories(&owner, "alice").await.unwrap();
    held.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected = vec![kept.clone(), forgotten.clone()];
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(held, expected, "forgotten memories are still held");

    assert_eq!(
        store
            .reassign_identity(&owner, "alice", "bob")
            .await
            .unwrap(),
        2
    );
    assert!(store
        .identity_memories(&owner, "alice")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store.identity_memories(&owner, "bob").await.unwrap().len(),
        3
    );
    assert_eq!(
        store
            .get(&kept)
            .await
            .unwrap()
            .unwrap()
            .identity_id
            .as_deref(),
        Some("bob")
    );

    assert_eq!(
        store.identity_memories(&other_agent, "bob").await.unwrap(),
        vec![bob_elsewhere.clone()],
        "another agent's memories of the identity reassigned to are not counted in"
    );
    assert_eq!(store.purge_identity(&owner, "bob").await.unwrap(), 3);
    assert_eq!(
        store.identity_memories(&other_agent, "bob").await.unwrap(),
        vec![bob_elsewhere.clone()],
        "another agent's memories of the purged identity are untouched"
    );
    assert!(store.get(&bob_elsewhere).await.unwrap().is_some());
    assert_eq!(
        store.identity_counts(&other_agent).await.unwrap(),
        BTreeMap::from([("alice".to_string(), 1), ("bob".to_string(), 1)])
    );
    assert!(
        store
            .identity_memories(&owner, "bob")
            .await
            .unwrap()
            .is_empty(),
        "purged, not hidden"
    );
    assert_eq!(
        store
            .identity_memories(&other_agent, "alice")
            .await
            .unwrap(),
        vec![elsewhere],
        "another agent's memories are untouched"
    );
    assert_eq!(
        store.identity_memories(&owner, "bobby").await.unwrap(),
        vec![neighbour],
        "an identity that only starts the same is untouched"
    );
    assert_eq!(store.identities(&owner).await.unwrap(), ["bobby"]);
}

#[cfg(test)]
mod recall_scope_tests {
    use super::*;
    use crate::types::MemoryLayer;

    fn mem(owner: &str, visibility: Visibility) -> Memory {
        Memory::new("note", MemoryLayer::Episodic, AgentId::new(owner)).with_visibility(visibility)
    }

    #[test]
    fn shared_scope_excludes_other_agents_private_memory() {
        // The Quill→Atlas incident: Quill's private owl note must not surface
        // in Atlas's recall, while Atlas's own notes and anything shared
        // (team/global) still do.
        let atlas = AgentId::new("atlas");
        let kept = retain_shared_recallable(
            &atlas,
            vec![
                mem("atlas", Visibility::Private),
                mem("quill", Visibility::Private),
                mem("quill", Visibility::Global),
                mem("quill", Visibility::Team("health".into())),
            ],
        );
        assert_eq!(kept.len(), 3);
        assert!(kept
            .iter()
            .all(|m| !(m.owner == AgentId::new("quill") && m.visibility == Visibility::Private)));
    }

    #[test]
    fn shared_scope_keeps_all_of_the_agents_own_memories() {
        let atlas = AgentId::new("atlas");
        let kept = retain_shared_recallable(
            &atlas,
            vec![
                mem("atlas", Visibility::Private),
                mem("atlas", Visibility::Global),
            ],
        );
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn shared_scope_filters_search_results() {
        let atlas = AgentId::new("atlas");
        let kept = retain_shared_recallable(
            &atlas,
            vec![
                MemorySearchResult {
                    memory: mem("atlas", Visibility::Private),
                    score: 0.9,
                },
                MemorySearchResult {
                    memory: mem("quill", Visibility::Private),
                    score: 0.8,
                },
            ],
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].memory.owner, atlas);
    }

    fn tagged(tags: &[&str]) -> Memory {
        Memory::new("m", crate::types::MemoryLayer::Episodic, AgentId::new("a"))
            .with_tags(tags.iter().map(|t| t.to_string()).collect())
    }

    #[test]
    fn required_tags_none_or_empty_is_noop() {
        let m = tagged(&["date:2026-07-05"]);
        assert!(passes_required_tags(&m, None));
        assert!(passes_required_tags(&m, Some(&[])));
    }

    #[test]
    fn required_tags_or_semantics_exact_match() {
        let m = tagged(&["date:2026-07-05", "source:passive"]);
        assert!(passes_required_tags(
            &m,
            Some(&["date:2026-07-05".to_string(), "pinned".to_string()])
        ));
        assert!(!passes_required_tags(&m, Some(&["pinned".to_string()])));
    }

    /// The day/month-transposition case that put May 7th's memories into the
    /// July 5th daily-notes prompt: Weaviate's word-tokenized tag match sees
    /// identical token sets, so the exactness guarantee must live here.
    #[test]
    fn required_tags_reject_transposed_date_tag() {
        let may = tagged(&["date:2026-05-07"]);
        assert!(!passes_required_tags(
            &may,
            Some(&["date:2026-07-05".to_string()])
        ));
    }
}

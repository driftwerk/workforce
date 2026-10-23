//! Graph-traversal queries layered on top of the backend primitives.
//!
//! BFS walks (callers-of, callees-of, impact analysis) live here at
//! the wrapper level; they call [`CodeGraphBackend::neighbours`]
//! through the trait object rather than poking at SQL directly, so
//! the same BFS code works for SQLite and Postgres without any
//! duplication.

use eyre::Result;
use std::collections::{HashSet, VecDeque};

use super::backend::EdgeDirection;
use super::entities::{CodeEntity, EntityId};
use super::relationships::RelationshipKind;
use super::CodeGraph;

/// Result of a graph query, including the entity and its distance from the root.
#[derive(Debug, Clone)]
pub struct GraphQueryResult {
    pub entity: CodeEntity,
    pub relationship: RelationshipKind,
    pub distance: usize,
}

/// How a [`ContextEntity`] entered the answer-ready context closure. Lets a
/// reader (LLM or human) see *why* each piece of code is present — the seed it
/// asked about, code that seed calls, code that calls the seed, or its tests /
/// trait implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReachedVia {
    /// A seed entity — directly resolved from the question.
    Seed,
    /// A sibling definition in the SAME FILE as a seed — the types, constants,
    /// and associated functions a reader needs to understand the seed, which the
    /// call graph alone can't represent (e.g. `Vault::encrypt_blob`'s file also
    /// holds `derive_key`, `SALT_LEN`/`NONCE_LEN`, and the on-disk-format
    /// doc-comment). Co-pulled to make the context COMPLETE.
    FileContext,
    /// Reached by following the seed's outgoing calls (depth ≤ 2).
    Callee,
    /// Reached by following the seed's direct callers (depth ≤ 1).
    Caller,
    /// Reached by following a `type_of` edge — a type referenced in the seed's
    /// signature (return/parameter type) defined elsewhere in the repo.
    Type,
    /// A test exercising one of the included entities.
    Test,
    /// An implementation of one of the included trait/interface entities.
    Impl,
}

impl ReachedVia {
    /// Lower-case label for rendering in assembled-context headers.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::FileContext => "file",
            Self::Callee => "callee",
            Self::Caller => "caller",
            Self::Type => "type",
            Self::Test => "test",
            Self::Impl => "impl",
        }
    }
}

/// An entity in the answer-ready context closure, tagged with how it was
/// reached. Pure graph metadata — carries no source body; the caller reads the
/// body from the indexed checkout using the entity's `file_path` + line span.
#[derive(Debug, Clone)]
pub struct ContextEntity {
    pub entity: CodeEntity,
    pub reached_via: ReachedVia,
}

/// Inclusive line span of an entity, used for budget accounting.
fn entity_line_span(entity: &CodeEntity) -> usize {
    entity.end_line.saturating_sub(entity.start_line) + 1
}

impl CodeGraph {
    /// Find all entities that call the given entity (reverse call graph).
    pub async fn callers_of(
        &self,
        entity_id: &EntityId,
        max_depth: usize,
    ) -> Result<Vec<GraphQueryResult>> {
        self.traverse_relationships(entity_id, "called_by", max_depth)
            .await
    }

    /// Find all entities that the given entity calls.
    pub async fn callees_of(
        &self,
        entity_id: &EntityId,
        max_depth: usize,
    ) -> Result<Vec<GraphQueryResult>> {
        self.traverse_relationships(entity_id, "calls", max_depth)
            .await
    }

    /// Find all entities that import from the given entity's file/module.
    pub async fn imported_by(
        &self,
        entity_id: &EntityId,
        max_depth: usize,
    ) -> Result<Vec<GraphQueryResult>> {
        self.traverse_relationships(entity_id, "imports", max_depth)
            .await
    }

    /// Find all entities that implement the given trait/interface.
    /// One-hop `Incoming` lookup — the source of an `implements` edge
    /// is the implementor, the target is the trait.
    pub async fn implementations_of(&self, entity_id: &EntityId) -> Result<Vec<GraphQueryResult>> {
        let entities = self
            .backend
            .neighbours(entity_id, "implements", EdgeDirection::Incoming)
            .await?;
        Ok(entities
            .into_iter()
            .map(|entity| GraphQueryResult {
                entity,
                relationship: RelationshipKind::Implements,
                distance: 1,
            })
            .collect())
    }

    /// Find all test entities associated with the given entity.
    pub async fn tests_for(&self, entity_id: &EntityId) -> Result<Vec<GraphQueryResult>> {
        let entities = self
            .backend
            .neighbours(entity_id, "tests_for", EdgeDirection::Incoming)
            .await?;
        Ok(entities
            .into_iter()
            .map(|entity| GraphQueryResult {
                entity,
                relationship: RelationshipKind::TestsFor,
                distance: 1,
            })
            .collect())
    }

    /// Find all entities that the given entity depends on.
    pub async fn depends_on(
        &self,
        entity_id: &EntityId,
        max_depth: usize,
    ) -> Result<Vec<GraphQueryResult>> {
        self.traverse_relationships(entity_id, "depends_on", max_depth)
            .await
    }

    /// BFS traversal through the relationship graph.
    /// Walks outward from the root entity following outgoing edges of
    /// the given kind (matches the historical semantics: always
    /// `source_id = current → target_id = next`).
    pub async fn traverse_relationships(
        &self,
        root_id: &EntityId,
        relationship_kind: &str,
        max_depth: usize,
    ) -> Result<Vec<GraphQueryResult>> {
        let mut results = Vec::new();
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();

        visited.insert(root_id.as_str().to_string());
        queue.push_back((root_id.clone(), 0usize));

        while let Some((current_id, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }

            let neighbours = self
                .backend
                .neighbours(&current_id, relationship_kind, EdgeDirection::Outgoing)
                .await?;

            for entity in neighbours {
                let entity_id_str = entity.id.as_str().to_string();
                if !visited.contains(&entity_id_str) {
                    visited.insert(entity_id_str);
                    queue.push_back((entity.id.clone(), depth + 1));
                    results.push(GraphQueryResult {
                        entity,
                        relationship: RelationshipKind::from_str(relationship_kind),
                        distance: depth + 1,
                    });
                }
            }
        }

        Ok(results)
    }

    /// Find all entities affected by a change to the given entity.
    /// This is the core of impact analysis: BFS through `called_by`,
    /// `imports`, `depends_on`, `implements`.
    pub async fn find_affected_entities(
        &self,
        entity_id: &EntityId,
        max_depth: usize,
    ) -> Result<Vec<GraphQueryResult>> {
        let mut results = Vec::new();
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();

        visited.insert(entity_id.as_str().to_string());
        queue.push_back((entity_id.clone(), 0usize));

        // Historical direction mapping — see the long comment in the
        // pre-refactor version of this function for the reasoning per
        // relationship kind. TL;DR: `called_by` is stored outward from
        // the callee so the "who is affected if A changes" walk is
        // `Outgoing`; the other three are stored inward (target is
        // the thing being depended on) so we look `Incoming`.
        let impact_kinds: [(&str, EdgeDirection); 4] = [
            ("called_by", EdgeDirection::Outgoing),
            ("imports", EdgeDirection::Incoming),
            ("depends_on", EdgeDirection::Incoming),
            ("implements", EdgeDirection::Incoming),
        ];

        while let Some((current_id, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }

            for (kind, direction) in impact_kinds {
                let neighbours = self
                    .backend
                    .neighbours(&current_id, kind, direction)
                    .await?;

                for entity in neighbours {
                    let eid = entity.id.as_str().to_string();
                    if !visited.contains(&eid) {
                        visited.insert(eid);
                        queue.push_back((entity.id.clone(), depth + 1));
                        results.push(GraphQueryResult {
                            entity,
                            relationship: RelationshipKind::from_str(kind),
                            distance: depth + 1,
                        });
                    }
                }
            }
        }

        Ok(results)
    }

    /// Same-file sibling definitions co-pulled per seed (after relevance
    /// ranking). Bounds the file-context expansion so a seed in a 60-definition
    /// module doesn't drag the whole file in. This per-seed count, combined with
    /// [`FILE_CONTEXT_TOP_SEEDS`], is the real backstop on how much file-context
    /// can grow — small enough that a critical helper (`derive_key`, beside
    /// `encrypt_blob`) makes the cut without flooding.
    const FILE_CONTEXT_PER_SEED: usize = 12;

    /// Only the top-N most relevant seeds receive same-file co-pull. Co-pull
    /// fans out per seed, so applying it to every seed of a multi-seed prose
    /// question (a dozen seeds × 8 siblings) floods the budget with the file
    /// context of marginal seeds. Restricting it to the strongest seeds keeps the
    /// expansion focused on the symbols the question is actually about.
    const FILE_CONTEXT_TOP_SEEDS: usize = 3;

    /// Definitions immediately adjacent (by line) to a co-pull seed that are
    /// included before the relevance-ranked siblings — the private helper that
    /// sits right beside the public method it serves (`derive_key` right before
    /// `encrypt_blob`). Counts toward [`FILE_CONTEXT_PER_SEED`].
    const FILE_CONTEXT_ADJACENT: usize = 4;

    /// Assemble the **answer-ready closure** of graph entities for a set of
    /// seeds: the code a reader needs to understand the seeds, gathered in one
    /// call instead of forcing the caller to chain `callees_of` / `callers_of`
    /// / `tests_for` / `implementations_of` by hand.
    ///
    /// For each seed we gather:
    ///   1. the seed itself,
    ///   2. its **same-file siblings** — the other key definitions (types,
    ///      constants, associated functions) living alongside the seed, which the
    ///      call graph structurally can't reach (`Vault::encrypt_blob`'s file also
    ///      holds `derive_key`, `SALT_LEN`/`NONCE_LEN`; `Scheduler`'s file holds
    ///      the `Schedule` enum and `JobPayload`). Co-pull is gated to the top
    ///      [`Self::FILE_CONTEXT_TOP_SEEDS`] seeds and the siblings are ranked by
    ///      relevance to the question `terms`, not by line proximity, so the
    ///      behaviour-bearing definitions win over adjacent field accessors,
    ///   3. its callees (outgoing `calls`, depth ≤ 2) — the code it runs,
    ///   4. its direct callers (depth ≤ 1) — who invokes it,
    ///   5. types referenced in its signature (`type_of`) and files it imports
    ///      (`imports`),
    ///   6. its tests (`tests_for`) and trait/interface implementations
    ///      (`implementations_of`).
    ///
    /// **False-call-edge damping**: the indexer resolves call targets by bare
    /// name (first match wins), so a method named `tick` can pick up a spurious
    /// `calls` edge to an unrelated `write`/`styles`/`add_xml` in a different
    /// crate. [`is_plausible_callee`] drops a callee when it is low-value or
    /// ambiguous and not anchored to a seed — generic names (`len`, `get`,
    /// `read`, …) unless qualified-tied to a seed, callees in an unrelated
    /// top-level subsystem, and ambiguous names whose collision resolved into a
    /// different module than the seed — so those false edges don't pollute the
    /// context. Genuine same-module callees always pass.
    ///
    /// Entities are deduped by id, first occurrence wins, so an entity that is
    /// both a seed and (say) a callee of another seed keeps its [`ReachedVia::Seed`]
    /// classification.
    ///
    /// `budget_lines` bounds the total inclusive line span. Seeds are *always*
    /// included even if they alone exceed the budget. The **file-context** — the
    /// same-file siblings, which are the proven completeness signal — fills next,
    /// against the full remaining budget (it is already bounded by
    /// [`FILE_CONTEXT_PER_SEED`] × [`FILE_CONTEXT_TOP_SEEDS`], so it cannot
    /// pathologically flood). The supplementary call-graph categories (callees,
    /// callers, types, tests/impls) then consume whatever budget is left, in that
    /// priority order. The question's `terms` decide which individual entities win
    /// within each category. `budget_lines == 0` yields exactly the seeds.
    ///
    /// `terms` are the salient question tokens (lower-cased nouns/verbs) used to
    /// rank siblings and break category ties; pass an empty slice to fall back to
    /// graph-order ranking. Pure graph traversal: no file reads — the caller
    /// resolves bodies afterwards. `repo_id` pins the same-file co-pull to one
    /// repo; the relationship traversals key off repo-unique entity ids.
    pub async fn full_context_for(
        &self,
        repo_id: &str,
        seeds: &[EntityId],
        terms: &[String],
        budget_lines: usize,
    ) -> Result<Vec<ContextEntity>> {
        let mut seen: HashSet<String> = HashSet::new();
        // Per-category candidate lists — kept separate so the budget pass can give
        // each its own slice instead of letting an early category drain the whole
        // budget.
        let mut seeds_out: Vec<ContextEntity> = Vec::new();
        let mut file_ctx: Vec<ContextEntity> = Vec::new();
        let mut callees: Vec<ContextEntity> = Vec::new();
        let mut callers: Vec<ContextEntity> = Vec::new();
        let mut types: Vec<ContextEntity> = Vec::new();
        let mut tests_impls: Vec<ContextEntity> = Vec::new();

        // 1. Seeds first — resolve each id to its entity (skip unknown ids). Seeds
        //    claim their id immediately so no later category can reclassify them.
        let mut seed_entities: Vec<CodeEntity> = Vec::new();
        for seed in seeds {
            if let Some(entity) = self.get_entity(seed).await? {
                if seen.insert(entity.id.as_str().to_string()) {
                    seed_entities.push(entity.clone());
                    seeds_out.push(ContextEntity {
                        entity,
                        reached_via: ReachedVia::Seed,
                    });
                }
            }
        }
        let seed_dirs: Vec<String> = seed_entities.iter().map(|e| dir_of(&e.file_path)).collect();

        // 2. Same-file siblings of the TOP seeds, ranked by relevance — kind +
        //    term match + substance (a 100-line method/enum carries behaviour a
        //    one-line accessor does not) — NOT line proximity, so the
        //    behaviour-bearing defs (`Schedule` enum, `JobPayload`, the `tick`
        //    loop) beat the adjacent constructors/accessors. Co-pull is gated to
        //    seeds that are themselves real defs in real files: a `module`/`file`
        //    seed (e.g. `pub mod scheduler;` in lib.rs) has no useful siblings and
        //    would waste a co-pull slot. Containers are excluded from the siblings
        //    too — a File entity spans the whole file.
        let copull_seeds: Vec<&CodeEntity> = seed_entities
            .iter()
            .filter(|e| is_def_kind(&e.kind))
            .take(Self::FILE_CONTEXT_TOP_SEEDS)
            .collect();
        for seed in copull_seeds {
            let siblings: Vec<CodeEntity> = self
                .find_entities_in_file(repo_id, &seed.file_path)
                .await?
                .into_iter()
                .filter(|e| e.id != seed.id && is_def_kind(&e.kind))
                .collect();

            // Two passes feed one ordered take(): first the seed's IMMEDIATE
            // neighbours by line, then the rest by relevance. The adjacency pass
            // captures the private helper that sits right beside the public method
            // it serves (`derive_key` immediately precedes `encrypt_blob`) — a real
            // code-organisation signal a global relevance sort buries under bigger,
            // higher-substance methods. The relevance pass then fills the remaining
            // slots with the behaviour-bearing / question-relevant defs
            // (`Schedule` enum, `tick`, `JobPayload`). Dedup across both passes is
            // the shared `seen` set.
            let mut by_line = siblings.clone();
            by_line.sort_by_key(|e| line_distance(e.start_line, seed.start_line));
            let mut by_relevance = siblings;
            by_relevance.sort_by(|a, b| {
                sibling_relevance(b, terms)
                    .total_cmp(&sibling_relevance(a, terms))
                    .then_with(|| {
                        line_distance(a.start_line, seed.start_line)
                            .cmp(&line_distance(b.start_line, seed.start_line))
                    })
            });
            let ranked = by_line
                .into_iter()
                .take(Self::FILE_CONTEXT_ADJACENT)
                .chain(by_relevance);

            let mut taken = 0usize;
            for sib in ranked {
                if taken >= Self::FILE_CONTEXT_PER_SEED {
                    break;
                }
                if seen.insert(sib.id.as_str().to_string()) {
                    file_ctx.push(ContextEntity {
                        entity: sib,
                        reached_via: ReachedVia::FileContext,
                    });
                    taken += 1;
                }
            }
        }

        // 3. Callees of every seed (depth ≤ 2), with false-edge damping.
        for seed in seeds {
            for result in self.callees_of(seed, 2).await? {
                if seen.contains(result.entity.id.as_str()) {
                    continue;
                }
                if self
                    .is_plausible_callee(repo_id, &result.entity, &seed_entities, &seed_dirs)
                    .await?
                    && seen.insert(result.entity.id.as_str().to_string())
                {
                    callees.push(ContextEntity {
                        entity: result.entity,
                        reached_via: ReachedVia::Callee,
                    });
                }
            }
        }

        // 4. Direct callers of every seed (depth ≤ 1).
        for seed in seeds {
            for result in self.callers_of(seed, 1).await? {
                if seen.insert(result.entity.id.as_str().to_string()) {
                    callers.push(ContextEntity {
                        entity: result.entity,
                        reached_via: ReachedVia::Caller,
                    });
                }
            }
        }

        // 5. Types referenced in each seed's signature (`type_of`) and the files
        //    it imports (`imports`).
        for seed in seeds {
            for kind in ["type_of", "imports"] {
                for result in self.traverse_relationships(seed, kind, 1).await? {
                    if seen.insert(result.entity.id.as_str().to_string()) {
                        types.push(ContextEntity {
                            entity: result.entity,
                            reached_via: ReachedVia::Type,
                        });
                    }
                }
            }
        }

        // 6. Tests + implementations of every seed (1-hop each).
        for seed in seeds {
            for result in self.tests_for(seed).await? {
                if seen.insert(result.entity.id.as_str().to_string()) {
                    tests_impls.push(ContextEntity {
                        entity: result.entity,
                        reached_via: ReachedVia::Test,
                    });
                }
            }
            for result in self.implementations_of(seed).await? {
                if seen.insert(result.entity.id.as_str().to_string()) {
                    tests_impls.push(ContextEntity {
                        entity: result.entity,
                        reached_via: ReachedVia::Impl,
                    });
                }
            }
        }

        // Budget pass. Seeds are unconditional. Non-seed categories are filled in
        // priority order — file-context FIRST, against the full budget, because
        // the same-file siblings are the proven completeness signal (the call
        // graph alone can't represent them); the call-graph categories are
        // SUPPLEMENTARY and take only the remaining budget. File-context can't
        // flood: it's bounded by FILE_CONTEXT_PER_SEED × FILE_CONTEXT_TOP_SEEDS.
        // Each entity is added greedily if it fits the global budget.
        let mut kept: Vec<ContextEntity> = Vec::new();
        let mut total: usize = 0;
        for ce in seeds_out {
            total += entity_line_span(&ce.entity);
            kept.push(ce);
        }
        let categories: [&[ContextEntity]; 5] =
            [&file_ctx, &callees, &callers, &types, &tests_impls];
        for bucket in categories {
            for ce in bucket {
                let span = entity_line_span(&ce.entity);
                if total + span <= budget_lines {
                    total += span;
                    kept.push(ce.clone());
                }
            }
        }

        Ok(kept)
    }

    /// Whether `callee` is a plausible call target of a seed, used to dampen the
    /// false `calls` edges the bare-name indexer materialises (a method named
    /// `tick` picks up a spurious edge to an unrelated `write`/`styles`/`add_xml`
    /// in a different crate, because resolution is first-match-on-bare-name). The
    /// goal is a TIGHT context: only genuinely-relevant callees survive.
    ///
    /// A callee is **dropped** when ANY of the following holds — unless it is
    /// anchored to a seed (same file, or a qualified name sharing the seed type's
    /// prefix), which always keeps it:
    ///   (a) **generic / common name** (`len`, `get`, `read`, `iter`, `push`,
    ///       `new`, `finish`, `info`, `error`, …) — these collide across the
    ///       whole repo, so a bare-name edge to one is almost always wrong;
    ///   (c) **unrelated subsystem** — its file shares no directory prefix with
    ///       any seed (a different top-level crate / module tree), so the edge
    ///       crossed a boundary the seed's code almost certainly doesn't;
    ///   (b) **ambiguous distant name** — the name is shared by another entity in
    ///       the repo AND the resolved target sits outside the seed's module, so
    ///       the bare-name resolver plausibly picked the wrong one.
    ///
    /// Same-module callees with specific names pass freely; we never drop a
    /// legitimate near-by call.
    async fn is_plausible_callee(
        &self,
        repo_id: &str,
        callee: &CodeEntity,
        seeds: &[CodeEntity],
        seed_dirs: &[String],
    ) -> Result<bool> {
        let callee_dir = dir_of(&callee.file_path);
        let same_module = seed_dirs.iter().any(|d| shares_dir_prefix(d, &callee_dir));

        // ANCHOR: a callee in a seed's own file, or whose qualified name shares a
        // seed's type/module qualifier, is genuinely the seed's code — keep it
        // regardless of how generic its name is (a `Scheduler::new` beside the
        // seed is real; a bare `new` in a distant crate is not).
        let anchored = seeds.iter().any(|s| {
            s.file_path == callee.file_path || shares_qualifier(&s.qualified_name, callee)
        });
        if anchored {
            return Ok(true);
        }

        // (a) Generic/common name not anchored to a seed → drop. These bare names
        //     collide everywhere; an unanchored edge to one is noise.
        if is_generic_name(&callee.name) {
            return Ok(false);
        }

        // (c) Unrelated subsystem (no shared directory prefix with any seed) →
        //     drop. The false docx/ooxml/aws edges all land here: the seed is in
        //     one crate, the collision resolved into another.
        if !same_module {
            return Ok(false);
        }

        // Same-module, specific name: keep unless it's an ambiguous name that
        // resolved into a DIFFERENT module than the seed — the bare-name resolver
        // could have picked the wrong same-crate `cron_next_run`. `find_entities_by_name`
        // also matches `qualified_name LIKE '%name'`, so request a generous limit
        // and count EXACT-name rows so qualified-suffix matches don't inflate the
        // collision count.
        let same_name = self
            .find_entities_by_name(repo_id, &callee.name, 50)
            .await?;
        let exact = same_name.iter().filter(|e| e.name == callee.name).count();
        if exact <= 1 {
            return Ok(true);
        }
        // Ambiguous, not anchored: the bare-name resolver picked ONE of several
        // same-named entities, and this one is in neither a seed's file nor its
        // type. When a same-named candidate DOES live in a seed file, the edge
        // should have resolved there — this resolved target is the wrong one (the
        // `cron_next_run`→heartbeat.rs collision: the real `cron_next_run` is in
        // the seed's scheduler.rs, but the edge points at heartbeat.rs). Drop it;
        // the correct same-file one, if genuinely called, arrives anchored.
        let real_lives_in_seed_file = same_name
            .iter()
            .filter(|e| e.name == callee.name)
            .any(|e| seeds.iter().any(|s| s.file_path == e.file_path));
        Ok(!real_lives_in_seed_file)
    }
}

/// Bare names so common across a repo that a first-match bare-name `calls` edge
/// to one is almost always a false positive — accessors, collection ops, and
/// log/builder verbs. Dropped as callees unless ANCHORED to a seed (same file /
/// shared qualifier), where they are genuinely the seed's own code.
fn is_generic_name(name: &str) -> bool {
    const GENERIC: &[&str] = &[
        "len",
        "get",
        "set",
        "read",
        "write",
        "iter",
        "push",
        "pop",
        "drop",
        "find",
        "matches",
        "is_empty",
        "load",
        "store",
        "new",
        "finish",
        "add",
        "info",
        "error",
        "warn",
        "debug",
        "next",
        "name",
        "id",
        "path",
        "value",
        "key",
        "build",
        "run",
        "init",
        "clone",
        "default",
        "from",
        "into",
        "to_string",
        "as_str",
        "len_of",
        "contains",
        "insert",
        "remove",
        "clear",
        "count",
        "first",
        "last",
        "take",
        "scope",
    ];
    let lower = name.to_lowercase();
    GENERIC.contains(&lower.as_str())
}

/// Whether `callee`'s qualified name shares the type/module qualifier of a
/// seed's qualified name — e.g. seed `Scheduler::tick` and callee
/// `Scheduler::persist_jobs` share the `Scheduler::` prefix, so the callee is
/// genuinely a method of the seed's type. The qualifier is everything up to the
/// final `::`; a bare (unqualified) name has none and never matches.
fn shares_qualifier(seed_qualified: &str, callee: &CodeEntity) -> bool {
    let Some((seed_prefix, _)) = seed_qualified.rsplit_once("::") else {
        return false;
    };
    if seed_prefix.is_empty() {
        return false;
    }
    callee
        .qualified_name
        .rsplit_once("::")
        .map(|(callee_prefix, _)| callee_prefix == seed_prefix)
        .unwrap_or(false)
}

/// Directory portion of a repo-relative path (`""` for a top-level file).
fn dir_of(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default()
}

/// Whether two directories share a meaningful prefix: one contains the other, or
/// they share their first two path segments (same crate + sub-module). Keeps
/// `crates/hub/src/scheduler.rs` and `crates/hub/src/heartbeat.rs` related while
/// separating `crates/hub/...` from `crates/agent-runtime/...`.
fn shares_dir_prefix(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return a == b;
    }
    if a.starts_with(b) || b.starts_with(a) {
        return true;
    }
    let segs = |s: &str| s.split('/').take(3).map(str::to_string).collect::<Vec<_>>();
    let (sa, sb) = (segs(a), segs(b));
    let common = sa.iter().zip(sb.iter()).take_while(|(x, y)| x == y).count();
    common >= 2
}

/// Absolute distance between two 1-indexed line numbers.
fn line_distance(a: usize, b: usize) -> usize {
    a.max(b) - a.min(b)
}

/// Relevance of a same-file sibling for the co-pull ranking. Higher wins.
/// Blends three signals so the definitions a reader actually needs lead over the
/// adjacent boilerplate:
///   * **kind** — data-model types (enum/struct/trait) and the data enum lead,
///     callables next, leaf consts/variants last,
///   * **term match** — name/qualified-name overlap with the question `terms`,
///   * **substance** — a 100-line method/enum (the `tick` loop, the `Schedule`
///     enum with all its variants) carries behaviour a 3-line constructor or
///     field accessor does not, so larger spans get a bounded bonus. This is what
///     floats `Scheduler::tick` (98 lines) above `Scheduler::new` (3 lines) when
///     neither matches the prose term "scheduler" lexically.
///
/// With no terms it degrades to kind + substance ranking.
fn sibling_relevance(entity: &CodeEntity, terms: &[String]) -> f32 {
    use crate::graph::entities::EntityKind::*;
    let kind_score = match entity.kind {
        Enum | Struct | Trait | Interface | Class | TypeAlias => 1.5,
        Function | Method | Macro => 1.2,
        Constant => 0.8,
        EnumVariant => 0.6,
        _ => 0.3,
    };
    // Match on the entity's OWN name only — NOT the qualified name. In a same-file
    // co-pull, a type's methods all share its qualified prefix (`Scheduler::tick`,
    // `Scheduler::insert_job`, …), so a term matching the seed type name would
    // flood the ranking toward that type's methods and bury sibling *types* that
    // don't share the prefix (the `Schedule` enum). Name-only keeps the data model
    // visible alongside the behaviour.
    let name = entity.name.to_lowercase();
    let mut term_score = 0.0_f32;
    for t in terms {
        let t = t.to_lowercase();
        if t.is_empty() {
            continue;
        }
        let this = if name == t {
            2.0
        } else if name.contains(&t) {
            1.2
        } else {
            0.0
        };
        term_score = term_score.max(this);
    }
    // Substance: log-ish bonus saturating at ~1.0 for a ~50+ line definition, so a
    // substantial method/enum edges out a trivial accessor without letting one
    // giant function dominate everything.
    let span = entity_line_span(entity) as f32;
    let substance = (span / 50.0).min(1.0);
    kind_score + term_score + substance
}

/// Whether an entity kind is a real definition worth co-pulling as file context
/// — types, callable items, constants, and aliases. Excludes containers
/// (`File`/`Module`, whose span is the whole file) and noise
/// (`Variable`/`Import`).
fn is_def_kind(kind: &crate::graph::entities::EntityKind) -> bool {
    use crate::graph::entities::EntityKind::*;
    matches!(
        kind,
        Function
            | Method
            | Struct
            | Class
            | Interface
            | Trait
            | Enum
            | EnumVariant
            | TypeAlias
            | Constant
            | Macro
            | Test
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::entities::{CodeVisibility, EntityKind, Language};
    use crate::graph::relationships::Relationship;

    fn make_entity(id: &str, name: &str, kind: EntityKind) -> CodeEntity {
        CodeEntity {
            id: EntityId(id.to_string()),
            repo_id: "test-repo".to_string(),
            kind,
            name: name.to_string(),
            qualified_name: name.to_string(),
            file_path: "test.rs".to_string(),
            start_line: 1,
            end_line: 10,
            signature: Some(format!("fn {name}()")),
            visibility: CodeVisibility::Public,
            parent_id: None,
            doc_comment: None,
            content_hash: "abc123".to_string(),
            language: Language::Rust,
        }
    }

    #[tokio::test]
    async fn callers_of_query() {
        let graph = CodeGraph::open_in_memory().unwrap();
        graph
            .register_repo("test-repo", "Test", "/tmp/test")
            .await
            .unwrap();

        let fn_a = make_entity("a", "fn_a", EntityKind::Function);
        let fn_b = make_entity("b", "fn_b", EntityKind::Function);
        let fn_c = make_entity("c", "fn_c", EntityKind::Function);

        graph.upsert_entities(&[fn_a, fn_b, fn_c]).await.unwrap();

        // b calls a, c calls a
        let rel1 = Relationship::new(
            EntityId("b".to_string()),
            EntityId("a".to_string()),
            RelationshipKind::Calls,
            1.0,
        );
        let rel2 = Relationship::new(
            EntityId("c".to_string()),
            EntityId("a".to_string()),
            RelationshipKind::Calls,
            1.0,
        );
        // Also create CalledBy relationships
        let rel3 = Relationship::new(
            EntityId("a".to_string()),
            EntityId("b".to_string()),
            RelationshipKind::CalledBy,
            1.0,
        );
        let rel4 = Relationship::new(
            EntityId("a".to_string()),
            EntityId("c".to_string()),
            RelationshipKind::CalledBy,
            1.0,
        );
        graph
            .upsert_relationships(&[rel1, rel2, rel3, rel4])
            .await
            .unwrap();

        let callers = graph
            .callers_of(&EntityId("a".to_string()), 2)
            .await
            .unwrap();
        assert_eq!(callers.len(), 2);

        let caller_names: Vec<&str> = callers.iter().map(|r| r.entity.name.as_str()).collect();
        assert!(caller_names.contains(&"fn_b"));
        assert!(caller_names.contains(&"fn_c"));
    }

    /// Entity with an explicit inclusive line span (`end_line - start_line + 1
    /// == span`) so budget tests can dial each entity's cost precisely. Each
    /// entity gets its OWN file so the same-file co-pull doesn't reclassify these
    /// call-graph fixtures as file-context — those tests exercise the call graph,
    /// not the sibling expansion (which has its own dedicated test).
    fn make_entity_span(id: &str, name: &str, span: usize) -> CodeEntity {
        let mut e = make_entity(id, name, EntityKind::Function);
        e.file_path = format!("{id}.rs");
        e.start_line = 1;
        e.end_line = span; // span lines: 1..=span inclusive
        e
    }

    /// `from` calls `to` — materialise both the forward `Calls` edge (callees)
    /// and the inverse `CalledBy` edge (callers), as the indexer does.
    fn calls(from: &str, to: &str) -> [Relationship; 2] {
        [
            Relationship::new(
                EntityId(from.to_string()),
                EntityId(to.to_string()),
                RelationshipKind::Calls,
                1.0,
            ),
            Relationship::new(
                EntityId(to.to_string()),
                EntityId(from.to_string()),
                RelationshipKind::CalledBy,
                1.0,
            ),
        ]
    }

    /// `test` is a `TestsFor` edge from the test entity to the entity under test
    /// (`implementations_of`/`tests_for` look at `Incoming` edges on the target).
    fn tests_for_edge(test: &str, target: &str) -> Relationship {
        Relationship::new(
            EntityId(test.to_string()),
            EntityId(target.to_string()),
            RelationshipKind::TestsFor,
            1.0,
        )
    }

    /// `implementor` implements `trait_id` — `Implements` edge from implementor
    /// to the trait/interface (matched `Incoming` on the trait by
    /// `implementations_of`).
    fn implements_edge(implementor: &str, trait_id: &str) -> Relationship {
        Relationship::new(
            EntityId(implementor.to_string()),
            EntityId(trait_id.to_string()),
            RelationshipKind::Implements,
            1.0,
        )
    }

    async fn graph_with(entities: &[CodeEntity], rels: &[Relationship]) -> CodeGraph {
        let graph = CodeGraph::open_in_memory().unwrap();
        graph
            .register_repo("test-repo", "Test", "/tmp/test")
            .await
            .unwrap();
        graph.upsert_entities(entities).await.unwrap();
        graph.upsert_relationships(rels).await.unwrap();
        graph
    }

    // Closure shape: a seed gathers its callee, its caller, a test, and a trait
    // impl — each tagged with the right `ReachedVia`, ordered seeds → callees →
    // callers → tests/impls.
    #[tokio::test]
    async fn full_context_includes_callees_callers_tests_impls_ordered() {
        let entities = vec![
            make_entity_span("seed", "do_work", 5),
            make_entity_span("callee", "helper", 5),
            make_entity_span("caller", "invoke", 5),
            make_entity_span("test", "test_do_work", 5),
            make_entity_span("impl", "WorkerImpl", 5),
        ];
        let mut rels = Vec::new();
        rels.extend(calls("seed", "callee")); // seed → callee
        rels.extend(calls("caller", "seed")); // caller → seed
        rels.push(tests_for_edge("test", "seed")); // test tests seed
        rels.push(implements_edge("impl", "seed")); // impl implements seed
        let graph = graph_with(&entities, &rels).await;

        let ctx = graph
            .full_context_for("test-repo", &[EntityId("seed".to_string())], &[], 10_000)
            .await
            .unwrap();

        let by_id: std::collections::HashMap<&str, ReachedVia> = ctx
            .iter()
            .map(|c| (c.entity.id.as_str(), c.reached_via))
            .collect();
        assert_eq!(by_id.get("seed"), Some(&ReachedVia::Seed));
        assert_eq!(by_id.get("callee"), Some(&ReachedVia::Callee));
        assert_eq!(by_id.get("caller"), Some(&ReachedVia::Caller));
        assert_eq!(by_id.get("test"), Some(&ReachedVia::Test));
        assert_eq!(by_id.get("impl"), Some(&ReachedVia::Impl));

        // Order: seed first, callee before caller, tests/impls last.
        let order: Vec<&str> = ctx.iter().map(|c| c.entity.id.as_str()).collect();
        assert_eq!(order[0], "seed");
        let pos = |id: &str| order.iter().position(|x| *x == id).unwrap();
        assert!(pos("callee") < pos("caller"));
        assert!(pos("caller") < pos("test"));
        assert!(pos("caller") < pos("impl"));
    }

    /// Entity in a specific file at a specific span — for the same-file co-pull
    /// and false-edge-damping tests where `file_path` is load-bearing.
    fn make_entity_in(
        id: &str,
        name: &str,
        kind: EntityKind,
        file: &str,
        start: usize,
        end: usize,
    ) -> CodeEntity {
        let mut e = make_entity(id, name, kind);
        e.file_path = file.to_string();
        e.start_line = start;
        e.end_line = end;
        e
    }

    // Same-file co-pull: a seed gathers the OTHER definitions in its file (the
    // types/consts/assoc-fns a reader needs), tagged `FileContext`, ordered right
    // after the seed; the whole-file `File`/`Module` containers are excluded.
    #[tokio::test]
    async fn full_context_copulls_same_file_siblings() {
        let entities = vec![
            make_entity_in(
                "seed",
                "encrypt_blob",
                EntityKind::Method,
                "vault.rs",
                40,
                50,
            ),
            make_entity_in(
                "derive",
                "derive_key",
                EntityKind::Function,
                "vault.rs",
                20,
                30,
            ),
            make_entity_in("salt", "SALT_LEN", EntityKind::Constant, "vault.rs", 5, 5),
            make_entity_in("file", "vault.rs", EntityKind::File, "vault.rs", 1, 999),
            // A definition in a DIFFERENT file must not be co-pulled.
            make_entity_in(
                "other",
                "unrelated",
                EntityKind::Function,
                "elsewhere.rs",
                1,
                3,
            ),
        ];
        let graph = graph_with(&entities, &[]).await;

        let ctx = graph
            .full_context_for("test-repo", &[EntityId("seed".to_string())], &[], 10_000)
            .await
            .unwrap();
        let by_id: std::collections::HashMap<&str, ReachedVia> = ctx
            .iter()
            .map(|c| (c.entity.id.as_str(), c.reached_via))
            .collect();

        assert_eq!(by_id.get("seed"), Some(&ReachedVia::Seed));
        assert_eq!(
            by_id.get("derive"),
            Some(&ReachedVia::FileContext),
            "same-file fn must be co-pulled"
        );
        assert_eq!(
            by_id.get("salt"),
            Some(&ReachedVia::FileContext),
            "same-file const must be co-pulled"
        );
        assert!(
            !by_id.contains_key("file"),
            "the whole-file File entity must be excluded (would blow the budget)"
        );
        assert!(
            !by_id.contains_key("other"),
            "a definition in a different file must not be co-pulled"
        );

        // Siblings ordered immediately after the seed, before any later kind.
        let order: Vec<&str> = ctx.iter().map(|c| c.entity.id.as_str()).collect();
        assert_eq!(order[0], "seed");
    }

    // False-call-edge damping (TIGHT policy): only genuinely-relevant callees
    // survive. A same-module specific-named callee is kept; the noise the
    // bare-name indexer produces is dropped — a generic name (`write`), an
    // ambiguous name resolving into a distant crate (docx `styles`), and an
    // unrelated-subsystem callee (a different top-level crate) all go.
    #[tokio::test]
    async fn full_context_dampens_false_call_edges() {
        let entities = vec![
            make_entity_in(
                "seed",
                "tick",
                EntityKind::Method,
                "crates/hub/src/scheduler.rs",
                10,
                20,
            ),
            // legit same-module, specific-named callee → kept.
            make_entity_in(
                "near",
                "next_run_time",
                EntityKind::Function,
                "crates/hub/src/scheduler.rs",
                30,
                40,
            ),
            // (a) generic name in a distant crate → dropped.
            make_entity_in(
                "falsewrite",
                "write",
                EntityKind::Function,
                "crates/agent-runtime/src/tools/report/native/docx.rs",
                1,
                10,
            ),
            // (c) specific name but in an unrelated subsystem (docx) → dropped.
            make_entity_in(
                "falsestyles",
                "styles",
                EntityKind::Function,
                "crates/agent-runtime/src/tools/report/native/docx.rs",
                20,
                30,
            ),
            // Another `styles` so the name is ambiguous in the repo, none in a
            // seed file → confirms the ambiguous-distant arm also rejects it.
            make_entity_in(
                "otherstyles",
                "styles",
                EntityKind::Function,
                "crates/agent-runtime/src/other.rs",
                1,
                10,
            ),
            // (c) specific name in a wholly different top-level crate → dropped.
            make_entity_in(
                "uniquefar",
                "compute_unique_thing",
                EntityKind::Function,
                "crates/other/src/lib.rs",
                1,
                10,
            ),
        ];
        let mut rels = Vec::new();
        rels.extend(calls("seed", "near"));
        rels.extend(calls("seed", "falsewrite"));
        rels.extend(calls("seed", "falsestyles"));
        rels.extend(calls("seed", "uniquefar"));
        let graph = graph_with(&entities, &rels).await;

        let ctx = graph
            .full_context_for("test-repo", &[EntityId("seed".to_string())], &[], 10_000)
            .await
            .unwrap();
        let ids: Vec<&str> = ctx.iter().map(|c| c.entity.id.as_str()).collect();
        assert!(ids.contains(&"near"), "same-module specific callee kept");
        assert!(
            !ids.contains(&"falsewrite"),
            "generic-named distant callee dropped (criterion a)"
        );
        assert!(
            !ids.contains(&"falsestyles"),
            "ambiguous distant callee dropped (criterion b/c)"
        );
        assert!(
            !ids.contains(&"uniquefar"),
            "unrelated-subsystem callee dropped (criterion c)"
        );
    }

    // Same-crate ambiguous-name false edge: a callee named `cron_next_run`
    // resolved (wrongly) into a sibling module while the REAL same-named def
    // lives in the seed's own file — the bare-name resolver picked the wrong one,
    // so the resolved (distant) target is dropped.
    #[tokio::test]
    async fn full_context_dampens_same_crate_ambiguous_misresolve() {
        let entities = vec![
            make_entity_in(
                "seed",
                "next_run_time",
                EntityKind::Function,
                "crates/hub/src/scheduler.rs",
                10,
                20,
            ),
            // The REAL cron_next_run, in the seed's own file (not called directly
            // here, just present so the name is ambiguous and a seed-file
            // candidate exists).
            make_entity_in(
                "real_cron",
                "cron_next_run",
                EntityKind::Function,
                "crates/hub/src/scheduler.rs",
                30,
                40,
            ),
            // The WRONG cron_next_run the edge resolved to — same crate, different
            // module. Must be dropped: a same-named def lives in the seed file.
            make_entity_in(
                "wrong_cron",
                "cron_next_run",
                EntityKind::Function,
                "crates/hub/src/heartbeat.rs",
                100,
                120,
            ),
        ];
        let mut rels = Vec::new();
        rels.extend(calls("seed", "wrong_cron"));
        let graph = graph_with(&entities, &rels).await;

        let ctx = graph
            .full_context_for("test-repo", &[EntityId("seed".to_string())], &[], 10_000)
            .await
            .unwrap();
        let ids: Vec<&str> = ctx.iter().map(|c| c.entity.id.as_str()).collect();
        assert!(
            !ids.contains(&"wrong_cron"),
            "ambiguous callee that misresolved away from the seed file is dropped"
        );
    }

    // A generic-named callee in a SEED's own file (anchored) is kept despite the
    // generic-name rule — it's genuinely the seed's own code; so is a callee in a
    // different file that shares the seed type's `::` qualifier.
    #[tokio::test]
    async fn full_context_keeps_anchored_generic_callee() {
        let mut seed = make_entity_in(
            "seed",
            "tick",
            EntityKind::Method,
            "crates/hub/src/scheduler.rs",
            10,
            20,
        );
        seed.qualified_name = "Scheduler::tick".to_string();
        // `new` is generic, but it's in the SEED's own file → anchored, kept.
        let samefile_new = make_entity_in(
            "samefile_new",
            "new",
            EntityKind::Method,
            "crates/hub/src/scheduler.rs",
            30,
            40,
        );
        // `persist_jobs` shares the seed's `Scheduler::` qualifier (different
        // file) → anchored, kept.
        let mut qualified = make_entity_in(
            "qualified",
            "persist_jobs",
            EntityKind::Method,
            "crates/hub/src/scheduler_persist.rs",
            1,
            10,
        );
        qualified.qualified_name = "Scheduler::persist_jobs".to_string();
        let entities = [seed, samefile_new, qualified];
        let mut rels = Vec::new();
        rels.extend(calls("seed", "samefile_new"));
        rels.extend(calls("seed", "qualified"));
        let graph = graph_with(&entities, &rels).await;

        let ctx = graph
            .full_context_for("test-repo", &[EntityId("seed".to_string())], &[], 10_000)
            .await
            .unwrap();
        let ids: Vec<&str> = ctx.iter().map(|c| c.entity.id.as_str()).collect();
        assert!(
            ids.contains(&"samefile_new"),
            "generic name in seed's own file is anchored → kept"
        );
        assert!(
            ids.contains(&"qualified"),
            "callee sharing the seed type's qualifier is anchored → kept"
        );
    }

    // Dedup, first-occurrence-wins: an entity that is both a seed and a callee
    // of another seed appears once, classified `Seed`.
    #[tokio::test]
    async fn full_context_dedups_first_occurrence_wins() {
        // seed_a → shared (callee); shared is itself a seed.
        let entities = vec![
            make_entity_span("seed_a", "a", 5),
            make_entity_span("shared", "shared", 5),
        ];
        let mut rels = Vec::new();
        rels.extend(calls("seed_a", "shared"));
        let graph = graph_with(&entities, &rels).await;

        let ctx = graph
            .full_context_for(
                "test-repo",
                &[
                    EntityId("seed_a".to_string()),
                    EntityId("shared".to_string()),
                ],
                &[],
                10_000,
            )
            .await
            .unwrap();

        let shared: Vec<&ContextEntity> = ctx
            .iter()
            .filter(|c| c.entity.id.as_str() == "shared")
            .collect();
        assert_eq!(shared.len(), 1, "deduped to a single entry");
        assert_eq!(
            shared[0].reached_via,
            ReachedVia::Seed,
            "seed classification wins over callee"
        );
    }

    // Budget caps non-seeds: a small budget drops the over-budget callee but the
    // first cheap one fits.
    #[tokio::test]
    async fn full_context_budget_caps_non_seeds_greedily() {
        // seed (span 5) + two callees: cheap (span 3) then big (span 100).
        let entities = vec![
            make_entity_span("seed", "s", 5),
            make_entity_span("cheap", "c", 3),
            make_entity_span("big", "b", 100),
        ];
        let mut rels = Vec::new();
        rels.extend(calls("seed", "cheap"));
        rels.extend(calls("seed", "big"));
        let graph = graph_with(&entities, &rels).await;

        // Budget 10: seed(5) is unconditional; the callee category fills greedily
        // against the global budget — cheap(3) → total 8 ≤ 10 fits; big(100) →
        // 108 > 10 is skipped (a cheap later entity could still fit after it).
        let ctx = graph
            .full_context_for("test-repo", &[EntityId("seed".to_string())], &[], 10)
            .await
            .unwrap();
        let ids: Vec<&str> = ctx.iter().map(|c| c.entity.id.as_str()).collect();
        assert!(ids.contains(&"seed"));
        assert!(ids.contains(&"cheap"));
        assert!(!ids.contains(&"big"), "over-budget callee dropped");
    }

    // Seeds are always present even when they alone blow the budget.
    #[tokio::test]
    async fn full_context_always_includes_seeds_over_budget() {
        let entities = vec![
            make_entity_span("seed1", "s1", 50),
            make_entity_span("seed2", "s2", 50),
            make_entity_span("callee", "c", 1),
        ];
        let mut rels = Vec::new();
        rels.extend(calls("seed1", "callee"));
        let graph = graph_with(&entities, &rels).await;

        // Budget 0: both seeds present (100 lines, way over), no non-seeds.
        let ctx = graph
            .full_context_for(
                "test-repo",
                &[EntityId("seed1".to_string()), EntityId("seed2".to_string())],
                &[],
                0,
            )
            .await
            .unwrap();
        let ids: Vec<&str> = ctx.iter().map(|c| c.entity.id.as_str()).collect();
        assert!(ids.contains(&"seed1"));
        assert!(ids.contains(&"seed2"));
        assert!(!ids.contains(&"callee"), "budget 0 → seeds only");
    }
}

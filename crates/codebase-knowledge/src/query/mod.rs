pub mod confidence;
pub mod explanation;
pub mod impact_analysis;
pub mod natural_language;

use eyre::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use workforce_common::breaker::Breaker;

use crate::freshness::{FreshnessResult, FreshnessTracker};
use crate::graph::entities::{CodeEntity, EntityId};
use crate::graph::CodeGraph;
use crate::vector::CodeVectorStore;

use self::confidence::{ConfidenceFactors, ConfidenceScore};
use self::explanation::{ExplanationAspect, ExplanationBuilder};
use self::impact_analysis::ImpactAnalyser;
use self::natural_language::QueryPlanner;

/// Result of a knowledge query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub answer: String,
    pub confidence: f32,
    pub sources: Vec<SourceReference>,
    pub freshness: String,
    pub suggestion: Option<String>,
    /// Why the answer was made without the repo's vectors, where a vector
    /// store is configured and did not serve: not complete yet, left alone
    /// after not answering, or not answering in time. `None` when it
    /// served, and when none is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vectors_skipped: Option<String>,
}

/// A reference to a source entity that contributed to the answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceReference {
    pub file: String,
    pub entity: String,
    pub kind: String,
    pub lines: String,
    pub relevance: f32,
}

/// Impact analysis result (simplified for tool output).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactResult {
    pub summary: String,
    pub affected_files: Vec<AffectedFile>,
    pub estimated_complexity: String,
    pub test_impact: Vec<String>,
    pub confidence: f32,
}

/// A file affected by a proposed change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedFile {
    pub path: String,
    pub reason: String,
    pub severity: String,
    pub entities_affected: Vec<String>,
}

/// A question pattern that a seed entity's NAME matched at word-boundary
/// strength or better (≥3.0), with that strength. Weaker tiers (buried
/// substring, qualified-name-only, doc/signature) are excluded — they carry no
/// specificity signal for the grounding decision.
#[derive(Debug, Clone)]
pub struct PatternHit {
    pub pattern_idx: usize,
    pub strength: f32,
}

/// A seed entity paired with the signals the injection decision reads: its
/// name-match quality, semantic score, the fused relevance it was ranked by, and
/// which question patterns its name actually matched.
#[derive(Debug, Clone)]
pub struct ScoredSeed {
    pub entity: CodeEntity,
    pub name_match: f32,
    pub vector: f32,
    pub relevance: f32,
    pub pattern_hits: Vec<PatternHit>,
}

/// Seed resolution across an agent's repos: the ranked seeds plus the corpus
/// statistics a caller weighs specificity with — how many repos were searched
/// and, per question pattern, in how many of them a symbol matched its name (its
/// document frequency). A pattern resolving in every repo (`read`, `errors`) is
/// generic; one resolving in a single repo (`undoSuspendAccount`) is specific.
#[derive(Debug, Clone)]
pub struct SeedResolution {
    pub seeds: Vec<ScoredSeed>,
    pub repo_count: usize,
    pub pattern_df: Vec<u16>,
    pub vector_configured: bool,
}

/// The main query engine for answering questions about codebases.
/// Performs multi-step retrieval: NL planning → entity search → graph traversal →
/// freshness check → live verification → answer synthesis.
pub struct QueryEngine {
    graph: Arc<CodeGraph>,
    vector_store: Option<Arc<dyn CodeVectorStore>>,
    /// Told when a question finds a repo's vectors not complete in the
    /// collection the store has: whoever fills them need not wait.
    vectors_wanted: Arc<tokio::sync::Notify>,
    /// Repos whose vectors are recorded complete and a search of which
    /// found none: for whoever keeps them to compare with the graph.
    vectors_suspected: Arc<Mutex<HashSet<String>>>,
    /// How long a question waits on the vector store, if it does not wait
    /// for as long as the store takes, and the breaker that leaves alone a
    /// store that keeps not answering in time.
    wait: Option<(Duration, Breaker)>,
    /// Why the last search of each repo's vectors did not serve, if it did
    /// not.
    unserved: Mutex<HashMap<String, String>>,
}

/// How long a question waits on the vector store before going on without
/// it, and for how long the store is not asked at all once
/// [`VECTOR_TRIPS_AFTER`] questions in a row have waited that out: a store
/// that accepts a request and never answers must not hold every question
/// up for as long as its client is prepared to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorWait {
    pub deadline: Duration,
    pub rest: Duration,
}

const LEFT_ALONE: &str =
    "the vector store is being left alone for a while, having not answered in time";

/// Times in a row the vector store does not answer in time before it is
/// left alone.
pub const VECTOR_TRIPS_AFTER: u32 = 3;

impl Default for VectorWait {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(3),
            rest: Duration::from_secs(30),
        }
    }
}

impl QueryEngine {
    pub fn new(graph: Arc<CodeGraph>) -> Self {
        Self {
            graph,
            vector_store: None,
            vectors_wanted: Arc::default(),
            vectors_suspected: Arc::default(),
            wait: None,
            unserved: Mutex::default(),
        }
    }

    /// Wait on the vector store as `wait` says. Without this a question
    /// waits for the store as long as the store's own client does, as
    /// questions of a Weaviate store always have.
    pub fn waiting(mut self, wait: VectorWait) -> Self {
        self.wait = Some((wait.deadline, Breaker::new(VECTOR_TRIPS_AFTER, wait.rest)));
        self
    }

    /// Repos whose vectors a search found absent though they are recorded
    /// complete. Whoever compares them with the graph takes them out.
    pub fn vectors_suspected(&self) -> Arc<Mutex<HashSet<String>>> {
        Arc::clone(&self.vectors_suspected)
    }

    /// Whether questions are asking the vector store (`"closed"`), leaving
    /// it alone after it kept not answering in time (`"open"`), or about
    /// to try it again (`"half_open"`); `None` where questions are given
    /// no deadline.
    pub fn vector_breaker(&self) -> Option<&'static str> {
        self.wait.as_ref().map(|(_, breaker)| breaker.told())
    }

    pub fn with_vector_store(
        graph: Arc<CodeGraph>,
        vector_store: Arc<dyn CodeVectorStore>,
    ) -> Self {
        Self {
            vector_store: Some(vector_store),
            ..Self::new(graph)
        }
    }

    /// Notified when a question finds a repo's vectors incomplete, or
    /// lost with their collection: for whoever fills them from the graph.
    pub fn vectors_wanted(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.vectors_wanted)
    }

    /// What the graph records of the repo's vectors, for a store that may
    /// lack them (see [`CodeVectorStore::tracks_completeness`]): the
    /// collection they are complete in, or why they are not to be used.
    /// Read from the graph, and never against the store's deadline.
    async fn recorded_complete(&self, repo_id: &str) -> std::result::Result<String, String> {
        match self.graph.vectors_complete_in(repo_id).await {
            Ok(Some(recorded)) => Ok(recorded),
            Ok(None) => Err(self.incomplete()),
            Err(e) => {
                tracing::warn!(
                    repo_id,
                    "could not tell whether the repo's vectors are complete: {e}"
                );
                Err(format!("the code graph could not be read: {e}"))
            }
        }
    }

    /// Say the repo's vectors are not complete, and tell whoever fills
    /// them.
    fn incomplete(&self) -> String {
        self.vectors_wanted.notify_one();
        "the repo's vectors are not complete yet".to_string()
    }

    /// The vector store, if one is configured and holds every entity of the
    /// repo; otherwise why it is not used. A store that may lack a repo's
    /// vectors is used only once the graph records them complete in the
    /// collection the store now has: until then, as after the collection
    /// was lost and made again, search and scoring go as with no store,
    /// rather than on part of the repo. So they do while a store that kept
    /// not answering in time is left alone.
    ///
    /// This asks the store which collection it has, within the deadline,
    /// and says nothing to the breaker either way: only a search of the
    /// vectors ([`Self::search_vectors`]) is what opens and closes it.
    async fn vectors_for(
        &self,
        repo_id: &str,
    ) -> std::result::Result<&Arc<dyn CodeVectorStore>, String> {
        let Some(vs) = self.vector_store.as_ref() else {
            return Err("no vector store is configured".to_string());
        };
        if !vs.tracks_completeness() {
            return Ok(vs);
        }
        let recorded = self.recorded_complete(repo_id).await?;
        let held = match &self.wait {
            None => vs.identity(false).await,
            Some((_, breaker)) if breaker.told() != "closed" => return Err(LEFT_ALONE.to_string()),
            Some((deadline, _)) => tokio::time::timeout(*deadline, vs.identity(false))
                .await
                .map_err(|_| format!("the vector store did not answer within {deadline:?}"))?,
        };
        match held {
            Ok(held) if held == recorded => Ok(vs),
            Ok(_) => Err(self.incomplete()),
            Err(e) => {
                tracing::warn!(
                    repo_id,
                    "could not tell whether the repo's vectors are complete: {e}"
                );
                Err(format!("the vector store failed: {e:#}"))
            }
        }
    }

    /// What the repo's vectors hold nearest `question`, or why they did
    /// not serve. One asking of the store: which collection it has, then
    /// the search, together within the deadline where there is one. That
    /// asking is what the breaker counts: not answered in time
    /// [`VECTOR_TRIPS_AFTER`] times in a row, the store is left alone for
    /// a rest, then tried by one question at a time.
    async fn search_vectors(
        &self,
        repo_id: &str,
        question: &str,
    ) -> std::result::Result<Vec<crate::vector::VectorSearchResult>, String> {
        let Some(vs) = self.vector_store.as_ref() else {
            return Err("no vector store is configured".to_string());
        };
        let failed = |e: eyre::Report| format!("the search of the vector store failed: {e:#}");
        if !vs.tracks_completeness() {
            let found = vs.search_entities(question, Some(repo_id), None, 20).await;
            return found.map_err(failed);
        }
        let recorded = self.recorded_complete(repo_id).await?;
        let asking = async {
            if vs.identity(false).await? != recorded {
                return eyre::Ok(None);
            }
            let found = vs
                .search_entities(question, Some(repo_id), None, 20)
                .await?;
            Ok(Some(found))
        };
        let asked = match &self.wait {
            None => asking.await,
            Some((deadline, breaker)) => {
                let Some(ticket) = breaker.admit() else {
                    return Err(LEFT_ALONE.to_string());
                };
                match tokio::time::timeout(*deadline, asking).await {
                    Ok(asked) => {
                        breaker.succeeded(ticket);
                        asked
                    }
                    Err(_) => {
                        let opened = breaker.failed(ticket);
                        tracing::warn!(
                            waited_ms = deadline.as_millis() as u64,
                            left_alone = opened,
                            rest_secs = breaker.rest().as_secs(),
                            "the vector store did not answer in time; the question goes on without it"
                        );
                        return Err(format!(
                            "the vector store did not answer within {deadline:?}"
                        ));
                    }
                }
            }
        };
        match asked {
            Ok(Some(found)) => Ok(found),
            Ok(None) => Err(self.incomplete()),
            Err(e) => Err(failed(e)),
        }
    }

    /// As [`Self::vectors_for`], without the reason.
    async fn ready_vectors(&self, repo_id: &str) -> Option<&Arc<dyn CodeVectorStore>> {
        self.vectors_for(repo_id).await.ok()
    }

    /// Why the repo's vectors did not serve the question just asked, if
    /// they did not: not ready, or the search of them failed or was not
    /// answered in time. `None` when they served, and when no vector store
    /// is configured at all.
    async fn vectors_unserved(&self, repo_id: &str) -> Option<String> {
        self.vector_store.as_ref()?;
        let searched = self
            .unserved
            .lock()
            .expect("not poisoned")
            .get(repo_id)
            .cloned();
        match searched {
            Some(why) => Some(why),
            None => self.vectors_for(repo_id).await.err(),
        }
    }

    /// Resolve the **seed entities** for a question: the most-relevant code
    /// entities found by the same vector + symbol retrieval `ask` performs.
    ///
    /// Runs symbol search (`search_entities`), exact name lookup
    /// (`find_entities_by_name`), and — when a vector store is configured —
    /// semantic vector search, against the patterns in `plan`. Results are
    /// deduped by id (first occurrence wins), scope-filtered when `scope` is
    /// given, and ranked so `plan.preferred_kinds` lead. Returns the ranked
    /// entities plus the best vector similarity score seen (`0.0` when no vector
    /// store or no hit), which `ask` folds into its confidence calculation.
    ///
    /// Shared by [`QueryEngine::ask`] and the answer-ready context assembler so
    /// both seed off the identical retrieval — there is exactly one definition
    /// of "the entities relevant to this question".
    pub async fn resolve_seed_entities(
        &self,
        repo_id: &str,
        question: &str,
        scope: Option<&str>,
        plan: &natural_language::RetrievalPlan,
    ) -> Result<(Vec<CodeEntity>, f32)> {
        let (matched_entities, vector_scores, best_vector_score) = self
            .gather_candidates(repo_id, question, scope, plan)
            .await?;

        // Rank by fused RELEVANCE: name-match quality + vector similarity + (for
        // explanation questions) a preference for behaviour-bearing kinds. This
        // replaces the old preferred-kinds partition, which was a no-op for prose
        // questions (their `preferred_kinds` is empty) and let leaf structs/consts
        // and files outrank the methods/enums that actually answer "how does X
        // work". Stable sort: equal scores keep retrieval order.
        let mut scored: Vec<(f32, CodeEntity)> = matched_entities
            .into_iter()
            .map(|e| {
                let vec_score = vector_scores.get(e.id.as_str()).copied().unwrap_or(0.0);
                (Self::seed_relevance(&e, plan, vec_score), e)
            })
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));

        Ok((
            scored.into_iter().map(|(_, e)| e).collect(),
            best_vector_score,
        ))
    }

    /// Resolve seed entities across **several repos at once**, returning one
    /// globally-ranked seed list (each carrying its own `repo_id`). The agent
    /// that asks "how does the scheduler work" may have several repos indexed
    /// (`driftwerk/workforce`, `driftwerk/orbit`, …); the relevant symbol can
    /// live in any of them. Candidates from every repo are scored with the SAME
    /// [`Self::seed_relevance`] and merged into one ranking, so the winning seeds
    /// come from whichever repo actually holds the code — not from a single repo
    /// guessed up front.
    ///
    /// The graph is one store keyed by `repo_id`, so this is per-repo gather then
    /// one global sort; entity ids are repo-unique, so the merge needs no extra
    /// dedup. A repo whose gather errors (transient store hiccup) is skipped
    /// rather than failing the whole resolution.
    ///
    /// Returns a [`SeedResolution`]: the seeds ranked by the FULL fused score
    /// (kind ordering still decides WHICH seeds lead) — each carrying its
    /// name-match, semantic score, and matched patterns — plus the corpus stats
    /// (`repo_count`, per-pattern `pattern_df`, `vector_configured`) the caller
    /// weighs specificity with when deciding whether, and from which repo, to
    /// ground.
    pub async fn resolve_seed_entities_multi(
        &self,
        repo_ids: &[String],
        question: &str,
        plan: &natural_language::RetrievalPlan,
    ) -> Result<SeedResolution> {
        let mut seeds: Vec<ScoredSeed> = Vec::new();
        let mut pattern_df: Vec<u16> = vec![0; plan.entity_patterns.len()];
        let mut repo_count: usize = 0;
        for repo_id in repo_ids {
            let (matched, vector_scores, _best) =
                match self.gather_candidates(repo_id, question, None, plan).await {
                    Ok(v) => v,
                    // One repo's store error must not sink a multi-repo agent's
                    // whole query — skip it and rank what the others returned.
                    Err(_) => continue,
                };
            repo_count += 1;
            // Which patterns found a matching symbol in THIS repo — the per-repo
            // contribution to each pattern's document frequency.
            let mut cleared = vec![false; plan.entity_patterns.len()];
            for e in matched {
                let hits = Self::name_match_by_pattern(&e, plan);
                for &(idx, _) in &hits {
                    cleared[idx] = true;
                }
                let vector = vector_scores.get(e.id.as_str()).copied().unwrap_or(0.0);
                let name_match = Self::name_match_score(&e, plan);
                let relevance = Self::seed_relevance(&e, plan, vector);
                let pattern_hits = hits
                    .into_iter()
                    .map(|(pattern_idx, strength)| PatternHit {
                        pattern_idx,
                        strength,
                    })
                    .collect();
                seeds.push(ScoredSeed {
                    entity: e,
                    name_match,
                    vector,
                    relevance,
                    pattern_hits,
                });
            }
            for (idx, hit) in cleared.into_iter().enumerate() {
                if hit {
                    pattern_df[idx] += 1;
                }
            }
        }
        seeds.sort_by(|a, b| b.relevance.total_cmp(&a.relevance));
        // Asked after searching: a search that found a collection gone has
        // it made again, empty, and the repo's vectors are then not ready.
        let mut vector_configured = self.vector_store.is_some() && !repo_ids.is_empty();
        for repo_id in repo_ids {
            vector_configured &= self.ready_vectors(repo_id).await.is_some();
        }
        Ok(SeedResolution {
            seeds,
            repo_count,
            pattern_df,
            vector_configured,
        })
    }

    /// Gather candidate entities for `question` in ONE repo via the three
    /// retrieval paths `ask` uses — symbol substring search, exact name lookup,
    /// and (when configured) vector semantic search — deduped by id with an
    /// optional `scope` path filter. Returns the candidates, their per-id vector
    /// scores, and the best vector score seen. Shared by the single- and
    /// multi-repo seed resolvers so both fold the identical retrieval into the
    /// same `seed_relevance` ranking — one definition of "candidates for this
    /// question in this repo".
    async fn gather_candidates(
        &self,
        repo_id: &str,
        question: &str,
        scope: Option<&str>,
        plan: &natural_language::RetrievalPlan,
    ) -> Result<(Vec<CodeEntity>, HashMap<String, f32>, f32)> {
        let mut matched_entities: Vec<CodeEntity> = Vec::new();
        let mut seen_ids = HashSet::new();
        // Per-entity vector similarity, keyed by id, folded into the relevance
        // ranking by the caller. Empty when no vector store is configured (the
        // symbol-only path), where ranking falls back to name-match + kind score.
        let mut vector_scores: HashMap<String, f32> = HashMap::new();

        // Symbol search across the extracted patterns.
        for pattern in &plan.entity_patterns {
            let results = self.graph.search_entities(repo_id, pattern, 20).await?;
            for entity in results {
                if seen_ids.insert(entity.id.as_str().to_string()) {
                    // Filter by scope if provided
                    if let Some(scope_path) = scope {
                        if !entity.file_path.starts_with(scope_path) {
                            continue;
                        }
                    }
                    matched_entities.push(entity);
                }
            }
        }

        // Also try exact name lookups
        for pattern in &plan.entity_patterns {
            let results = self
                .graph
                .find_entities_by_name(repo_id, pattern, 10)
                .await?;
            for entity in results {
                if seen_ids.insert(entity.id.as_str().to_string()) {
                    if let Some(scope_path) = scope {
                        if !entity.file_path.starts_with(scope_path) {
                            continue;
                        }
                    }
                    matched_entities.push(entity);
                }
            }
        }

        // Vector semantic search (if available)
        let mut best_vector_score: f32 = 0.0;
        if let Some(vs) = self.vector_store.as_ref() {
            let found = self.search_vectors(repo_id, question).await;
            {
                let mut last = self.unserved.lock().expect("not poisoned");
                match &found {
                    Err(why) => last.insert(repo_id.to_string(), why.clone()),
                    Ok(_) => last.remove(repo_id),
                };
            }
            if let Ok(vector_results) = found {
                if vector_results.is_empty() && vs.tracks_completeness() {
                    // Recorded complete, and nothing of the repo found: the
                    // store may have lost it, so it is compared with the
                    // graph by whoever keeps it.
                    let mut suspected = self.vectors_suspected.lock().expect("not poisoned");
                    suspected.insert(repo_id.to_string());
                    self.vectors_wanted.notify_one();
                }
                for vr in vector_results {
                    if vr.score > best_vector_score {
                        best_vector_score = vr.score;
                    }
                    // Record the score for ranking regardless of whether the
                    // entity also came back from symbol search (first/highest
                    // wins — vector results arrive best-first).
                    vector_scores
                        .entry(vr.entity_id.clone())
                        .or_insert(vr.score);
                    if seen_ids.insert(vr.entity_id.clone()) {
                        let entity_id = EntityId(vr.entity_id.clone());
                        if let Ok(Some(entity)) = self.graph.get_entity(&entity_id).await {
                            if let Some(scope_path) = scope {
                                if !entity.file_path.starts_with(scope_path) {
                                    continue;
                                }
                            }
                            matched_entities.push(entity);
                        }
                    }
                }
            }
        }

        Ok((matched_entities, vector_scores, best_vector_score))
    }

    /// Name-match quality (0–5): how well the entity's own name matches a
    /// question pattern, the dominant — and *relevance-vs-noise* — component of
    /// [`Self::seed_relevance`]. An exact token match (5.0) beats a prefix/suffix
    /// match (3.5) beats a word-boundary match (3.0) beats a buried substring
    /// (2.0) beats a qualified-name-only match (1.0) beats a hit that landed only
    /// via doc-comment/signature text (0.3). So a `Scheduler::tick` method
    /// outranks an unrelated `network` function whose name merely contains the
    /// `work` content token.
    ///
    /// Split out of `seed_relevance` so the answer-ready context assembler can
    /// gate on this term ALONE — the kind/vector components are ranking nudges
    /// among already-relevant seeds, not signals of whether the question names
    /// real code, so they must not decide the inject-or-not gate.
    /// Strength of a single lower-cased `pattern` against an entity's lower-cased
    /// name and qualified name, on the 0–5 ladder: exact 5.0 / prefix|suffix 3.5 /
    /// word-boundary 3.0 / buried substring 2.0 / qualified-name-only 1.0 /
    /// doc-or-signature-only 0.3. An empty pattern scores 0.0.
    fn pattern_strength(name_lower: &str, qualified_lower: &str, p: &str) -> f32 {
        if p.is_empty() {
            0.0
        } else if name_lower == p {
            5.0
        } else if name_lower.starts_with(p) || name_lower.ends_with(p) {
            3.5
        } else if Self::has_word_boundary_match(name_lower, p) {
            3.0
        } else if name_lower.contains(p) {
            2.0
        } else if qualified_lower.contains(p) {
            1.0
        } else {
            // Matched only via doc-comment / signature text (search_entities
            // also searches those) — weakest signal.
            0.3
        }
    }

    fn name_match_score(entity: &CodeEntity, plan: &natural_language::RetrievalPlan) -> f32 {
        let name_lower = entity.name.to_lowercase();
        let qualified_lower = entity.qualified_name.to_lowercase();
        plan.entity_patterns
            .iter()
            .map(|pattern| {
                Self::pattern_strength(&name_lower, &qualified_lower, &pattern.to_lowercase())
            })
            .fold(0.0_f32, f32::max)
    }

    /// The question patterns this entity's NAME matched at word-boundary strength
    /// or better (≥3.0), each paired with its strength — the per-pattern view
    /// [`Self::name_match_score`] reduces to a max. Weaker tiers are dropped: an
    /// incidental buried substring or doc-comment hit is not evidence the question
    /// names this symbol, so it carries no specificity signal for grounding.
    fn name_match_by_pattern(
        entity: &CodeEntity,
        plan: &natural_language::RetrievalPlan,
    ) -> Vec<(usize, f32)> {
        let name_lower = entity.name.to_lowercase();
        let qualified_lower = entity.qualified_name.to_lowercase();
        plan.entity_patterns
            .iter()
            .enumerate()
            .filter_map(|(idx, pattern)| {
                let s =
                    Self::pattern_strength(&name_lower, &qualified_lower, &pattern.to_lowercase());
                (s >= 3.0).then_some((idx, s))
            })
            .collect()
    }

    /// Fused relevance score for a candidate seed entity. Higher is better.
    ///
    /// Three additive components, in descending weight:
    ///   * **name-match quality** (0–5): [`Self::name_match_score`] — how well the
    ///     entity's own name matches a question pattern. The dominant term.
    ///   * **kind score** (0–~1.5): for explanation questions, behaviour-bearing
    ///     kinds (function/method/enum/trait) lead, structs/classes sit in the
    ///     middle, and bare modules/files/constants/variables/imports trail —
    ///     "how does X work" wants the code that *does* something. Driven by
    ///     `plan.is_explanation`, not the keyword-matched `preferred_kinds` (which
    ///     is empty for prose). When `preferred_kinds` *is* populated (the
    ///     question literally named a kind) those kinds get an extra nudge.
    ///   * **vector similarity** (0–~1): the semantic score when a vector store is
    ///     configured; `0.0` on the symbol-only path.
    fn seed_relevance(
        entity: &CodeEntity,
        plan: &natural_language::RetrievalPlan,
        vector_score: f32,
    ) -> f32 {
        let name_score = Self::name_match_score(entity, plan);

        // Kind score: prefer behaviour-bearing code for explanation questions.
        let kind = entity.kind.as_str();
        let mut kind_score: f32 = if plan.is_explanation {
            match kind {
                "function" | "method" => 1.5,
                "enum" | "trait" | "interface" => 1.2,
                "struct" | "class" => 0.8,
                "type_alias" | "macro" => 0.6,
                "constant" | "enum_variant" => 0.3,
                // Modules/files are containers, not behaviour; variables/imports
                // are noise for "how does it work".
                _ => 0.0,
            }
        } else {
            0.0
        };
        // Honour an explicitly named kind ("which struct…", "what function…").
        if plan.preferred_kinds.iter().any(|k| k == kind) {
            kind_score += 0.5;
        }

        name_score + kind_score + vector_score
    }

    /// Whether `pattern` appears in `haystack` bounded by non-alphanumeric
    /// separators (or `_`), i.e. as a whole word/segment rather than buried
    /// inside a larger identifier. `derive_key` matches `key` here; `keyword`
    /// does not.
    fn has_word_boundary_match(haystack: &str, pattern: &str) -> bool {
        let is_sep = |c: char| !c.is_alphanumeric();
        haystack.match_indices(pattern).any(|(idx, _)| {
            let before_ok = idx == 0 || haystack[..idx].chars().next_back().is_some_and(is_sep);
            let after = idx + pattern.len();
            let after_ok =
                after >= haystack.len() || haystack[after..].chars().next().is_some_and(is_sep);
            before_ok && after_ok
        })
    }

    /// Answer a natural language question about the codebase.
    /// Uses the QueryPlanner to convert the question into a retrieval plan,
    /// then executes each step to gather context before synthesising an answer.
    pub async fn ask(
        &self,
        repo_id: &str,
        question: &str,
        scope: Option<&str>,
        thorough: bool,
    ) -> Result<QueryResult> {
        // Step 1: Plan retrieval
        let plan = QueryPlanner::plan(question, thorough);

        // Step 2: Resolve seed entities via the shared vector + symbol retrieval.
        // Already deduped, scope-filtered, and ranked (preferred kinds first).
        let (matched_entities, best_vector_score) = self
            .resolve_seed_entities(repo_id, question, scope, &plan)
            .await?;
        let vectors_skipped = self.vectors_unserved(repo_id).await;

        if matched_entities.is_empty() {
            return Ok(QueryResult {
                answer: format!(
                    "No entities found matching your question. Searched for: {}. \
                     Try using code.search for text-level search or be more specific about \
                     entity names (use backticks or quotes around identifiers).",
                    plan.entity_patterns.join(", ")
                ),
                confidence: 0.0,
                sources: vec![],
                freshness: "unknown".to_string(),
                suggestion: Some(
                    "Try using code.search for text-level search, or knowledge.graph for direct graph queries".to_string(),
                ),
                vectors_skipped,
            });
        }

        // Step 4: Perform graph traversals for the top entities
        let mut traversal_context = Vec::new();
        let top_entities: Vec<&CodeEntity> = matched_entities.iter().take(5).collect();

        for traversal in &plan.graph_traversals {
            for entity in &top_entities {
                let results = self
                    .graph
                    .traverse_relationships(
                        &entity.id,
                        &traversal.relationship,
                        traversal.max_depth,
                    )
                    .await?;
                for result in results {
                    let line = format!(
                        "  - {} `{}` {} `{}` ({}:{})",
                        result.entity.kind.as_str(),
                        result.entity.name,
                        result.relationship.as_str(),
                        entity.name,
                        result.entity.file_path,
                        result.entity.start_line,
                    );
                    traversal_context.push(line);
                }
            }
        }

        // Step 5: Freshness check
        let tracker = FreshnessTracker::new(self.graph.clone());
        let mut _fresh_count = 0;
        let mut stale_count = 0;
        let mut unknown_count = 0;

        for entity in top_entities.iter().take(3) {
            match tracker.check_entity(&entity.id).await {
                Ok(FreshnessResult::Fresh { .. }) => _fresh_count += 1,
                Ok(FreshnessResult::Stale { .. }) => stale_count += 1,
                Ok(FreshnessResult::Unknown) | Err(_) => unknown_count += 1,
            }
        }

        let freshness = if stale_count > 0 {
            format!(
                "partial — {stale_count} of {} sources may be stale",
                top_entities.len()
            )
        } else if unknown_count == top_entities.len() {
            "unknown — live verification unavailable".to_string()
        } else {
            "fresh".to_string()
        };

        // Step 6: Live verification (thorough mode)
        let mut live_verified = false;
        if plan.verify_live {
            // Read actual source for top-3 entities and verify hashes
            for entity in top_entities.iter().take(3) {
                if let Ok(content) = tokio::fs::read_to_string(&entity.file_path).await {
                    let lines: Vec<&str> = content.lines().collect();
                    let start = entity.start_line.saturating_sub(1);
                    let end = entity.end_line.min(lines.len());
                    if start < end {
                        let source = lines[start..end].join("\n");
                        let live_hash = blake3::hash(source.as_bytes()).to_hex()[..16].to_string();
                        if live_hash == entity.content_hash {
                            live_verified = true;
                        }
                    }
                }
            }
        }

        // Step 7: Build confidence score
        let confidence = ConfidenceScore::calculate(ConfidenceFactors {
            source_freshness: if stale_count == 0 { 1.0 } else { 0.5 },
            vector_similarity: if vectors_skipped.is_none() && self.vector_store.is_some() {
                if best_vector_score > 0.0 {
                    best_vector_score
                } else {
                    0.3
                }
            } else {
                0.7 // Graph-only fallback — no vector store configured
            },
            graph_completeness: if traversal_context.is_empty() {
                0.6
            } else {
                0.9
            },
            live_verified,
            corroboration: if matched_entities.len() > 1 { 0.8 } else { 0.5 },
        });

        // Step 8: Gather summaries for matched entities
        let mut summary_parts = Vec::new();
        for entity in top_entities.iter().take(5) {
            if let Ok(Some((text, is_stale))) = self.graph.get_summary(&entity.id, "function").await
            {
                let stale_marker = if is_stale { " (stale)" } else { "" };
                summary_parts.push(format!(
                    "**{}** `{}`{}: {}",
                    entity.kind.as_str(),
                    entity.qualified_name,
                    stale_marker,
                    text
                ));
            }
        }

        // Step 9: Synthesise answer
        let mut answer_parts = Vec::new();

        answer_parts.push(format!(
            "Found {} relevant entities:",
            matched_entities.len()
        ));

        for entity in matched_entities.iter().take(8) {
            let sig = entity
                .signature
                .as_deref()
                .map(|s| format!(" — `{}`", s))
                .unwrap_or_default();
            let doc = entity
                .doc_comment
                .as_deref()
                .map(|d| format!("\n    {}", d.lines().next().unwrap_or("")))
                .unwrap_or_default();
            answer_parts.push(format!(
                "- {} `{}` in `{}` (lines {}-{}){}{doc}",
                entity.kind.as_str(),
                entity.qualified_name,
                entity.file_path,
                entity.start_line,
                entity.end_line,
                sig,
            ));
        }

        if !summary_parts.is_empty() {
            answer_parts.push("\n**Summaries**:".to_string());
            answer_parts.extend(summary_parts);
        }

        if !traversal_context.is_empty() {
            answer_parts.push(format!(
                "\n**Related entities** ({}):",
                traversal_context.len()
            ));
            answer_parts.extend(traversal_context.into_iter().take(15));
        }

        let sources: Vec<SourceReference> = matched_entities
            .iter()
            .take(10)
            .map(|e| SourceReference {
                file: e.file_path.clone(),
                entity: e.qualified_name.clone(),
                kind: e.kind.as_str().to_string(),
                lines: format!("{}-{}", e.start_line, e.end_line),
                relevance: if plan
                    .entity_patterns
                    .iter()
                    .any(|p| e.name.contains(p) || e.qualified_name.contains(p))
                {
                    1.0
                } else {
                    0.6
                },
            })
            .collect();

        let suggestion = if confidence.is_reliable() {
            Some("Use knowledge.explain for deeper analysis of specific entities".to_string())
        } else {
            Some("Low confidence — try code.search for more targeted text search, or provide more specific entity names".to_string())
        };

        Ok(QueryResult {
            answer: answer_parts.join("\n"),
            confidence: confidence.overall,
            sources,
            freshness,
            suggestion,
            vectors_skipped,
        })
    }

    /// Explain a specific entity.
    pub async fn explain(
        &self,
        entity_id: &EntityId,
        aspect: Option<&str>,
    ) -> Result<explanation::Explanation> {
        let builder = ExplanationBuilder::new(self.graph.clone());
        let aspect = aspect
            .map(ExplanationAspect::from_str)
            .unwrap_or(ExplanationAspect::Full);
        builder.explain(entity_id, &aspect).await
    }

    /// Perform impact analysis for a proposed change.
    pub async fn impact(
        &self,
        entity_id: &EntityId,
        change_description: &str,
        max_depth: usize,
    ) -> Result<ImpactResult> {
        let analyser = ImpactAnalyser::new(self.graph.clone());
        let report = analyser
            .analyse(entity_id, change_description, max_depth)
            .await?;

        let affected_files: Vec<AffectedFile> = report
            .affected_files
            .iter()
            .map(|f| AffectedFile {
                path: f.path.clone(),
                reason: f.reason.clone(),
                severity: f.severity.as_str().to_string(),
                entities_affected: f.entities.clone(),
            })
            .collect();

        Ok(ImpactResult {
            summary: report.summary,
            affected_files,
            estimated_complexity: report.estimated_complexity,
            test_impact: report.affected_tests,
            confidence: report.confidence,
        })
    }

    /// Generate an architecture overview for a scope.
    ///
    /// `detail` controls depth: `"summary"` (default) produces stats + key abstractions +
    /// module map; `"full"` adds pattern detection and follow-up suggestions.
    pub async fn overview(
        &self,
        repo_id: &str,
        scope: Option<&str>,
        detail: &str,
    ) -> Result<QueryResult> {
        use crate::graph::entities::{CodeVisibility, EntityKind};
        use std::collections::{BTreeMap, HashMap};

        let full = detail == "full";

        // Get all entities (filtered by scope if provided)
        let entities = if let Some(scope_path) = scope {
            let all = self.graph.all_entities(repo_id).await?;
            all.into_iter()
                .filter(|e| e.file_path.starts_with(scope_path))
                .collect::<Vec<_>>()
        } else {
            self.graph.all_entities(repo_id).await?
        };

        if entities.is_empty() {
            return Ok(QueryResult {
                answer: "No entities found in the specified scope. Run knowledge.index first."
                    .to_string(),
                confidence: 0.0,
                sources: vec![],
                freshness: "unknown".to_string(),
                suggestion: Some("Use knowledge.index trigger to start indexing".to_string()),
                vectors_skipped: None,
            });
        }

        let mut answer_parts = Vec::new();
        let scope_label = scope.unwrap_or("full repository");

        // ── Section 1: Stats header ──
        let stats = self.graph.get_stats(repo_id).await?;

        // Language breakdown
        let mut lang_counts: HashMap<String, usize> = HashMap::new();
        for e in &entities {
            if matches!(e.kind, EntityKind::File) || matches!(e.kind, EntityKind::Module) {
                continue;
            }
            *lang_counts
                .entry(e.language.as_str().to_string())
                .or_insert(0) += 1;
        }
        let mut lang_list: Vec<_> = lang_counts.into_iter().collect();
        lang_list.sort_by(|a, b| b.1.cmp(&a.1));
        let lang_str = lang_list
            .iter()
            .take(5)
            .map(|(l, c)| format!("{l}: {c}"))
            .collect::<Vec<_>>()
            .join(", ");

        answer_parts.push(format!(
            "**Architecture overview** ({scope_label}): {} entities, {} relationships, {} files",
            stats.entity_count, stats.relationship_count, stats.file_count,
        ));
        if !lang_str.is_empty() {
            answer_parts.push(format!("**Languages**: {lang_str}"));
        }
        answer_parts.push(String::new());

        // ── Section 2: Key abstractions (traits/interfaces + implementations) ──
        let traits = self
            .graph
            .find_entities_by_kind(repo_id, "trait", 100)
            .await?;
        let interfaces = self
            .graph
            .find_entities_by_kind(repo_id, "interface", 100)
            .await?;

        // Filter by scope if needed
        let traits: Vec<_> = if let Some(sp) = scope {
            traits
                .into_iter()
                .filter(|e| e.file_path.starts_with(sp))
                .collect()
        } else {
            traits
        };
        let interfaces: Vec<_> = if let Some(sp) = scope {
            interfaces
                .into_iter()
                .filter(|e| e.file_path.starts_with(sp))
                .collect()
        } else {
            interfaces
        };

        // Gather implementation counts for each abstraction
        let mut abstractions: Vec<(String, String, Option<String>, Vec<String>)> = Vec::new();
        for entity in traits.iter().chain(interfaces.iter()) {
            let impls = self.graph.implementations_of(&entity.id).await?;
            let impl_names: Vec<String> = impls.iter().map(|r| r.entity.name.clone()).collect();
            abstractions.push((
                entity.kind.as_str().to_string(),
                entity.name.clone(),
                entity.signature.clone(),
                impl_names,
            ));
        }
        // Sort by implementation count (most implementations first)
        abstractions.sort_by(|a, b| b.3.len().cmp(&a.3.len()));

        if !abstractions.is_empty() {
            answer_parts.push(format!(
                "**Key Abstractions** ({} traits, {} interfaces):",
                traits.len(),
                interfaces.len()
            ));
            for (kind, name, sig, impls) in abstractions.iter().take(10) {
                let sig_str = sig
                    .as_deref()
                    .map(|s| format!(" — `{s}`"))
                    .unwrap_or_default();
                if impls.is_empty() {
                    answer_parts.push(format!("- {kind} `{name}`{sig_str}"));
                } else {
                    answer_parts.push(format!(
                        "- {kind} `{name}` ({} impl{}){sig_str}",
                        impls.len(),
                        if impls.len() == 1 { "" } else { "s" }
                    ));
                    let display_impls: Vec<_> = impls.iter().take(6).collect();
                    let suffix = if impls.len() > 6 {
                        format!(", +{} more", impls.len() - 6)
                    } else {
                        String::new()
                    };
                    answer_parts.push(format!(
                        "  -> {}{}",
                        display_impls
                            .iter()
                            .map(|n| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        suffix
                    ));
                }
            }
            if abstractions.len() > 10 {
                answer_parts.push(format!(
                    "  ... and {} more abstractions",
                    abstractions.len() - 10
                ));
            }
            answer_parts.push(String::new());
        }

        // ── Section 3: Architectural patterns (full mode only) ──
        if full {
            let mut pattern_parts = Vec::new();

            // Service/Client/Provider pattern
            let service_entities: Vec<&CodeEntity> = entities
                .iter()
                .filter(|e| {
                    matches!(e.kind, EntityKind::Struct | EntityKind::Class)
                        && (e.name.ends_with("Service")
                            || e.name.ends_with("Client")
                            || e.name.ends_with("Provider"))
                })
                .collect();
            if !service_entities.is_empty() {
                let names: Vec<_> = service_entities
                    .iter()
                    .take(8)
                    .map(|e| format!("`{}`", e.name))
                    .collect();
                let suffix = if service_entities.len() > 8 {
                    format!(", +{} more", service_entities.len() - 8)
                } else {
                    String::new()
                };
                pattern_parts.push(format!(
                    "- **Service layer**: {}{}",
                    names.join(", "),
                    suffix
                ));
            }

            // Error types
            let error_entities: Vec<&CodeEntity> = entities
                .iter()
                .filter(|e| {
                    matches!(
                        e.kind,
                        EntityKind::Struct | EntityKind::Enum | EntityKind::Class
                    ) && (e.name.ends_with("Error") || e.name.ends_with("Exception"))
                })
                .collect();
            if !error_entities.is_empty() {
                let dirs: HashSet<_> = error_entities
                    .iter()
                    .filter_map(|e| e.file_path.rsplit_once('/').map(|(d, _)| d))
                    .collect();
                pattern_parts.push(format!(
                    "- **Error handling**: {} error types across {} modules",
                    error_entities.len(),
                    dirs.len()
                ));
            }

            // Configuration types
            let config_entities: Vec<&CodeEntity> = entities
                .iter()
                .filter(|e| {
                    matches!(e.kind, EntityKind::Struct | EntityKind::Class)
                        && (e.name.ends_with("Config")
                            || e.name.ends_with("Settings")
                            || e.name.ends_with("Options"))
                })
                .collect();
            if !config_entities.is_empty() {
                let names: Vec<_> = config_entities
                    .iter()
                    .take(8)
                    .map(|e| format!("`{}`", e.name))
                    .collect();
                let suffix = if config_entities.len() > 8 {
                    format!(", +{} more", config_entities.len() - 8)
                } else {
                    String::new()
                };
                pattern_parts.push(format!(
                    "- **Configuration**: {}{}",
                    names.join(", "),
                    suffix
                ));
            }

            // Handler/Strategy pattern (traits with 3+ impls, already computed)
            let handler_patterns: Vec<_> = abstractions
                .iter()
                .filter(|(_, _, _, impls)| impls.len() >= 3)
                .collect();
            if !handler_patterns.is_empty() {
                let names: Vec<_> = handler_patterns
                    .iter()
                    .take(5)
                    .map(|(_, name, _, impls)| format!("`{name}` ({})", impls.len()))
                    .collect();
                pattern_parts.push(format!(
                    "- **Handler/Strategy**: {} with 3+ implementations",
                    names.join(", ")
                ));
            }

            // Test coverage
            let test_count = entities
                .iter()
                .filter(|e| matches!(e.kind, EntityKind::Test))
                .count();
            let tests_for_count = self
                .graph
                .count_relationships_by_kind(repo_id, "tests_for")
                .await
                .unwrap_or(0);
            if test_count > 0 {
                pattern_parts.push(format!(
                    "- **Test coverage**: {} test entities, {} test-target links",
                    test_count, tests_for_count
                ));
            }

            if !pattern_parts.is_empty() {
                answer_parts.push("**Detected Patterns**:".to_string());
                answer_parts.extend(pattern_parts);
                answer_parts.push(String::new());
            }
        }

        // ── Section 4: Module map (top directories by entity count) ──
        let mut dir_entities: BTreeMap<String, Vec<&CodeEntity>> = BTreeMap::new();
        for entity in &entities {
            let dir = entity
                .file_path
                .rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_else(|| ".".to_string());
            dir_entities.entry(dir).or_default().push(entity);
        }

        // Sort by entity count descending, take top 15
        let mut sorted_dirs: Vec<_> = dir_entities.into_iter().collect();
        sorted_dirs.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
        let total_dirs = sorted_dirs.len();

        answer_parts.push(format!(
            "**Module Map** ({total_dirs} directories, showing top {}):",
            total_dirs.min(15)
        ));

        for (dir, dir_ents) in sorted_dirs.iter().take(15) {
            let mut kind_counts: HashMap<&str, usize> = HashMap::new();
            for e in dir_ents {
                *kind_counts.entry(e.kind.as_str()).or_insert(0) += 1;
            }
            // Sort kind counts for consistent output
            let mut kinds_vec: Vec<_> = kind_counts.into_iter().collect();
            kinds_vec.sort_by(|a, b| b.1.cmp(&a.1));
            let kinds_str: String = kinds_vec
                .iter()
                .map(|(k, v)| format!("{v} {k}s"))
                .collect::<Vec<_>>()
                .join(", ");

            // Inline module summary if available
            let module_summary = {
                let mut summary = None;
                for e in dir_ents {
                    if matches!(e.kind, EntityKind::Module | EntityKind::File) {
                        if let Ok(Some((text, _))) = self.graph.get_summary(&e.id, "module").await {
                            summary = Some(text);
                            break;
                        }
                    }
                }
                summary
            };

            answer_parts.push(format!("**`{dir}/`** — {kinds_str}"));
            if let Some(ref summary) = module_summary {
                answer_parts.push(format!("  {summary}"));
            }

            // List key types (structs, traits, interfaces, classes, public functions)
            let key_entities: Vec<&&CodeEntity> = dir_ents
                .iter()
                .filter(|e| {
                    matches!(
                        e.kind,
                        EntityKind::Struct
                            | EntityKind::Trait
                            | EntityKind::Interface
                            | EntityKind::Class
                    ) || (matches!(e.kind, EntityKind::Function | EntityKind::Method)
                        && matches!(e.visibility, CodeVisibility::Public))
                })
                .take(5)
                .collect();

            for entity in key_entities {
                let sig = entity
                    .signature
                    .as_deref()
                    .map(|s| format!(" — `{s}`"))
                    .unwrap_or_default();
                answer_parts.push(format!(
                    "  - {} `{}`{sig}",
                    entity.kind.as_str(),
                    entity.name,
                ));
            }
        }

        if total_dirs > 15 {
            answer_parts.push(format!("  ... and {} more directories", total_dirs - 15));
        }

        // ── Section 5: Follow-up suggestions (full mode only) ──
        if full {
            answer_parts.push(String::new());
            answer_parts.push("**Suggested queries**:".to_string());

            // Suggest exploring the most-implemented abstraction
            if let Some((kind, name, _, impls)) = abstractions.first() {
                if !impls.is_empty() {
                    answer_parts.push(format!(
                        "- `knowledge.explain` entity=\"{name}\" aspect=\"dependencies\" — understand the core {kind}",
                    ));
                }
            }

            // Suggest scoped overview for the largest directory
            if let Some((dir, _)) = sorted_dirs.first() {
                if scope.is_none() {
                    answer_parts.push(format!(
                        "- `knowledge.overview` scope=\"{dir}\" — deep-dive into the largest module",
                    ));
                }
            }

            // Suggest graph query for a key abstraction
            if let Some((_, name, _, _)) = abstractions
                .iter()
                .find(|(_, _, _, impls)| !impls.is_empty())
            {
                answer_parts.push(format!(
                    "- `knowledge.graph` query=\"implementations_of\" entity=\"{name}\" — see all implementors",
                ));
            }
        }

        let sources: Vec<SourceReference> = sorted_dirs
            .iter()
            .take(10)
            .map(|(dir, _)| SourceReference {
                file: dir.clone(),
                entity: format!("{dir}/"),
                kind: "directory".to_string(),
                lines: "".to_string(),
                relevance: 1.0,
            })
            .collect();

        let suggestion = if full {
            None // Suggestions are already inline
        } else {
            Some(
                "Use detail=\"full\" for pattern detection and follow-up suggestions, \
                 or knowledge.ask for specific questions"
                    .to_string(),
            )
        };

        Ok(QueryResult {
            answer: answer_parts.join("\n"),
            confidence: 0.8,
            sources,
            freshness: "graph_based".to_string(),
            suggestion,
            vectors_skipped: None,
        })
    }

    /// Execute a direct graph query.
    pub async fn graph_query(
        &self,
        query_type: &str,
        entity_id: &EntityId,
        depth: usize,
    ) -> Result<Vec<GraphQueryResultDto>> {
        let results = match query_type {
            "callers_of" => self.graph.callers_of(entity_id, depth).await?,
            "callees_of" => self.graph.callees_of(entity_id, depth).await?,
            "imports_of" | "imported_by" => self.graph.imported_by(entity_id, depth).await?,
            "implementations_of" => self.graph.implementations_of(entity_id).await?,
            "tests_for" => self.graph.tests_for(entity_id).await?,
            "depends_on" => self.graph.depends_on(entity_id, depth).await?,
            "affected_by" => self.graph.find_affected_entities(entity_id, depth).await?,
            _ => {
                return Err(eyre::eyre!(
                    "Unknown graph query type: {}. Valid types: callers_of, callees_of, \
                     imports_of, implementations_of, tests_for, depends_on, affected_by",
                    query_type
                ));
            }
        };

        Ok(results
            .into_iter()
            .map(|r| GraphQueryResultDto {
                name: r.entity.name,
                qualified_name: r.entity.qualified_name,
                kind: r.entity.kind.as_str().to_string(),
                file: r.entity.file_path,
                start_line: r.entity.start_line,
                end_line: r.entity.end_line,
                relationship: r.relationship.as_str().to_string(),
                distance: r.distance,
            })
            .collect())
    }
}

/// Serializable graph query result for tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphQueryResultDto {
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub relationship: String,
    pub distance: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::entities::{CodeVisibility, EntityKind, Language};
    use crate::graph::relationships::{Relationship, RelationshipKind};

    fn make_entity(id: &str, name: &str, kind: EntityKind, file: &str) -> CodeEntity {
        CodeEntity {
            id: EntityId(id.to_string()),
            repo_id: "test".to_string(),
            kind,
            name: name.to_string(),
            qualified_name: format!("crate::{name}"),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 10,
            signature: Some(format!("fn {name}()")),
            visibility: CodeVisibility::Public,
            parent_id: None,
            doc_comment: Some(format!("Does {name} things")),
            content_hash: "abc".to_string(),
            language: Language::Rust,
        }
    }

    #[tokio::test]
    async fn ask_finds_entities() {
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        graph.register_repo("test", "Test", "/tmp").await.unwrap();

        let entities = vec![
            make_entity(
                "a",
                "process_message",
                EntityKind::Function,
                "src/handler.rs",
            ),
            make_entity(
                "b",
                "validate_input",
                EntityKind::Function,
                "src/validation.rs",
            ),
            make_entity("c", "MessageHandler", EntityKind::Struct, "src/handler.rs"),
        ];
        graph.upsert_entities(&entities).await.unwrap();

        let engine = QueryEngine::new(graph);
        let result = engine
            .ask("test", "How does process_message work?", None, false)
            .await
            .unwrap();

        assert!(result.answer.contains("process_message"));
        assert!(!result.sources.is_empty());
        assert!(result.confidence > 0.0);
    }

    #[tokio::test]
    async fn ask_with_scope() {
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        graph.register_repo("test", "Test", "/tmp").await.unwrap();

        let entities = vec![
            make_entity("a", "foo", EntityKind::Function, "src/module_a/handler.rs"),
            make_entity("b", "foo", EntityKind::Function, "src/module_b/handler.rs"),
        ];
        graph.upsert_entities(&entities).await.unwrap();

        let engine = QueryEngine::new(graph);
        let result = engine
            .ask("test", "What does foo do?", Some("src/module_a"), false)
            .await
            .unwrap();

        // Should only find entities in module_a
        assert!(result
            .sources
            .iter()
            .all(|s| s.file.starts_with("src/module_a")));
    }

    #[tokio::test]
    async fn graph_query_callers() {
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        graph.register_repo("test", "Test", "/tmp").await.unwrap();

        let entities = vec![
            make_entity("a", "target_fn", EntityKind::Function, "a.rs"),
            make_entity("b", "caller_fn", EntityKind::Function, "b.rs"),
        ];
        graph.upsert_entities(&entities).await.unwrap();

        let rel = Relationship::new(
            EntityId("a".to_string()),
            EntityId("b".to_string()),
            RelationshipKind::CalledBy,
            1.0,
        );
        graph.upsert_relationships(&[rel]).await.unwrap();

        let engine = QueryEngine::new(graph);
        let results = engine
            .graph_query("callers_of", &EntityId("a".to_string()), 2)
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "caller_fn");
    }

    #[tokio::test]
    async fn overview_returns_structure() {
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        graph.register_repo("test", "Test", "/tmp").await.unwrap();

        let entities = vec![
            make_entity("a", "Handler", EntityKind::Struct, "src/handler.rs"),
            make_entity("b", "process", EntityKind::Function, "src/handler.rs"),
            make_entity("c", "Validator", EntityKind::Struct, "src/validation.rs"),
        ];
        graph.upsert_entities(&entities).await.unwrap();

        let engine = QueryEngine::new(graph);
        let result = engine.overview("test", None, "summary").await.unwrap();

        assert!(result.answer.contains("Architecture overview"));
        assert!(result.answer.contains("Handler"));
    }

    /// An entity stamped with a specific `repo_id` so the per-repo
    /// `repo_id`-filtered queries (`search_entities`/`find_entities_by_name`)
    /// return it only for its own repo.
    fn make_entity_in_repo(
        id: &str,
        name: &str,
        kind: EntityKind,
        file: &str,
        repo_id: &str,
    ) -> CodeEntity {
        let mut e = make_entity(id, name, kind, file);
        e.repo_id = repo_id.to_string();
        e
    }

    // Multi-repo seed resolution: with two repos indexed in one store, a
    // question whose relevant symbol lives only in repo B resolves its seeds from
    // repo B — each seed carries repo B's `repo_id` — even though repo A is
    // searched too. This is the core of cross-repo `code.context`.
    #[tokio::test]
    async fn resolve_seed_entities_multi_picks_repo_with_the_code() {
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        graph
            .register_repo("repo-a", "orbit", "/checkouts/orbit")
            .await
            .unwrap();
        graph
            .register_repo("repo-b", "workforce", "/checkouts/workforce")
            .await
            .unwrap();

        // Repo A: unrelated code. Repo B: the scheduler the question is about.
        let entities = vec![
            make_entity_in_repo(
                "a1",
                "render_invoice",
                EntityKind::Function,
                "src/billing.rs",
                "repo-a",
            ),
            make_entity_in_repo(
                "b1",
                "scheduler_tick",
                EntityKind::Function,
                "src/scheduler.rs",
                "repo-b",
            ),
            make_entity_in_repo(
                "b2",
                "ScheduledJob",
                EntityKind::Struct,
                "src/scheduler.rs",
                "repo-b",
            ),
        ];
        graph.upsert_entities(&entities).await.unwrap();

        let engine = QueryEngine::new(graph);
        let plan = QueryPlanner::plan("how does scheduler_tick work?", false);
        let resolution = engine
            .resolve_seed_entities_multi(
                &["repo-a".to_string(), "repo-b".to_string()],
                "how does scheduler_tick work?",
                &plan,
            )
            .await
            .unwrap();
        let seeds = &resolution.seeds;

        assert!(!seeds.is_empty(), "should resolve a seed across the repos");
        // An exact symbol-name question's best name-match is 5.0 even with no
        // vector store — the specificity the grounding decision reads.
        let best_name_match = seeds.iter().map(|s| s.name_match).fold(0.0_f32, f32::max);
        assert!(
            best_name_match >= 5.0,
            "exact-name seed's name-match should be 5.0: {best_name_match}"
        );
        // The top seed is the scheduler function, and it carries repo B's id.
        assert_eq!(seeds[0].entity.name, "scheduler_tick");
        assert_eq!(
            seeds[0].entity.repo_id, "repo-b",
            "winning seed comes from the repo that holds the code"
        );
        // The unrelated repo-a billing fn must not outrank it (it shouldn't match
        // the scheduler patterns at all).
        assert!(
            seeds.iter().all(|s| s.entity.name != "render_invoice"),
            "an unrelated symbol from the other repo must not be a seed"
        );
    }

    // Enumeration order doesn't change the winner: resolving with repo B listed
    // first yields the same top seed (the global sort is by score, not by repo
    // order).
    #[tokio::test]
    async fn resolve_seed_entities_multi_is_order_independent() {
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        graph
            .register_repo("repo-a", "orbit", "/checkouts/orbit")
            .await
            .unwrap();
        graph
            .register_repo("repo-b", "workforce", "/checkouts/workforce")
            .await
            .unwrap();
        let entities = vec![
            make_entity_in_repo(
                "a1",
                "render_invoice",
                EntityKind::Function,
                "src/billing.rs",
                "repo-a",
            ),
            make_entity_in_repo(
                "b1",
                "scheduler_tick",
                EntityKind::Function,
                "src/scheduler.rs",
                "repo-b",
            ),
        ];
        graph.upsert_entities(&entities).await.unwrap();
        let engine = QueryEngine::new(graph);
        let plan = QueryPlanner::plan("how does scheduler_tick work?", false);

        let resolution = engine
            .resolve_seed_entities_multi(
                &["repo-b".to_string(), "repo-a".to_string()],
                "how does scheduler_tick work?",
                &plan,
            )
            .await
            .unwrap();
        assert_eq!(resolution.seeds[0].entity.name, "scheduler_tick");
        assert_eq!(resolution.seeds[0].entity.repo_id, "repo-b");
    }

    // Per-pattern document frequency reflects how many repos hold a matching
    // symbol: a name present in both repos has df 2 (generic across the corpus),
    // one present in a single repo has df 1 (specific). This is the signal the
    // grounding decision weighs so a generic token cannot alone trigger injection.
    #[tokio::test]
    async fn resolve_seed_entities_multi_reports_pattern_df_and_vector_flag() {
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        graph
            .register_repo("repo-a", "orbit", "/checkouts/orbit")
            .await
            .unwrap();
        graph
            .register_repo("repo-b", "workforce", "/checkouts/workforce")
            .await
            .unwrap();
        let entities = vec![
            make_entity_in_repo(
                "a1",
                "handle_request",
                EntityKind::Function,
                "src/a.rs",
                "repo-a",
            ),
            make_entity_in_repo(
                "b1",
                "handle_request",
                EntityKind::Function,
                "src/b.rs",
                "repo-b",
            ),
            make_entity_in_repo(
                "b2",
                "scheduler_tick",
                EntityKind::Function,
                "src/scheduler.rs",
                "repo-b",
            ),
        ];
        graph.upsert_entities(&entities).await.unwrap();
        let engine = QueryEngine::new(graph);
        let question = "how does handle_request and scheduler_tick work?";
        let plan = QueryPlanner::plan(question, false);
        let resolution = engine
            .resolve_seed_entities_multi(
                &["repo-a".to_string(), "repo-b".to_string()],
                question,
                &plan,
            )
            .await
            .unwrap();

        assert_eq!(resolution.repo_count, 2);
        assert!(!resolution.vector_configured, "no vector store configured");
        assert_eq!(resolution.pattern_df.len(), plan.entity_patterns.len());

        let df = |name: &str| {
            let idx = plan
                .entity_patterns
                .iter()
                .position(|p| p == name)
                .unwrap_or_else(|| panic!("pattern {name} should be extracted"));
            resolution.pattern_df[idx]
        };
        assert_eq!(
            df("handle_request"),
            2,
            "name in both repos → df 2 (generic)"
        );
        assert_eq!(
            df("scheduler_tick"),
            1,
            "name in one repo → df 1 (specific)"
        );
    }
}

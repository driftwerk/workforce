//! Answer-ready code context: given a question, assemble the **complete
//! relevant source** in one operation instead of forcing the LLM to chain
//! `knowledge.ask` → `knowledge.symbol` → `knowledge.graph` → `repo.read`
//! (which it does badly — hallucinating, under-scoping).
//!
//! The symbol graph stores spans + signatures but *no body*. This module joins
//! the two halves a developer reads together:
//!   1. resolve the **seed entities** for the question with the same vector +
//!      symbol retrieval `knowledge.ask` uses ([`QueryEngine::resolve_seed_entities`]),
//!   2. expand to the answer-ready graph closure — seed + callees + callers +
//!      tests + implementations, budget-bounded ([`CodeGraph::full_context_for`]),
//!   3. read each entity's *actual body* from the indexed checkout at HEAD
//!      (reusing `repo_search`'s containment-checked reader + commit stamp),
//!   4. assemble one clearly-labelled block, dropping identical bodies and
//!      respecting the line budget.
//!
//! Increment 1 is the callable + testable core (plus the `code.context` tool);
//! it is *not* yet wired into the answer pipeline.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::json;

use workforce_agent::tools::filesystem::{Grep, ReadFile};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_codebase_knowledge::graph::entities::{CodeEntity, EntityId, EntityKind};
use workforce_codebase_knowledge::graph::{ContextEntity, ReachedVia};
use workforce_codebase_knowledge::query::natural_language::{QueryPlanner, RetrievalPlan};
use workforce_codebase_knowledge::query::{ScoredSeed, SeedResolution};
use workforce_core::code_citation::CodeCitation;
use workforce_projects::RepoSource;

use crate::knowledge::access_guard::{maybe_guard, KnowledgeAccessGuard};
use crate::knowledge::repo_search::head_commit;
use crate::knowledge::{KnowledgeService, MaintenanceRunner};

/// Number of top-ranked entities used as closure seeds. Larger than the
/// `top_entities` cap `knowledge.ask` traverses from (5): a prose question like
/// "how does the scheduler work" legitimately spans several behaviour-bearing
/// symbols (`Scheduler::tick`, the `Schedule` enum, `ScheduledJob`,
/// `SchedulerTool`), and the relevance ranking now floats those into the top
/// dozen. Seeding the closure off all of them — then letting `full_context_for`'s
/// line budget bound the result — is what makes the assembled context COMPLETE
/// without flooding. `ask`'s breadth is unchanged; only this assembler widens.
const SEED_LIMIT: usize = 12;

/// Default line budget for the assembled block when the caller doesn't specify
/// one — enough for a function plus its immediate call graph and a test or two,
/// without flooding the context window.
pub const DEFAULT_BUDGET_LINES: usize = 1500;

/// Full weight for an identifier-shaped question pattern (snake_case,
/// camel/PascalCase, digits, path separators, or any explicit capitalisation) —
/// a deliberate code reference, trusted as such. A bare lowercase English word
/// is weighted far lower (see [`GroundingConfig::bare_word_shape_weight`]).
const IDENTIFIER_SHAPE_WEIGHT: f32 = 1.0;

/// Tunable thresholds for the grounding decision — whether, and from which
/// repo(s), to inject — read live from `CodeContextConfig` so they retune without
/// a redeploy. The defaults are calibrated so a conversational question that only
/// brushes a symbol name ("staging gets write capability") does NOT inject while
/// a question naming a specific symbol ("how does `RateLimitGate` work") does; see
/// [`decide`].
#[derive(Debug, Clone, Copy)]
pub struct GroundingConfig {
    /// Minimum specificity-weighted score the primary repo must reach — the
    /// corroboration path (several rare tokens agreeing, or one very rare one).
    pub min_grounding_score: f32,
    /// A single identifier-shaped strong match (strength × shape, IDF-independent)
    /// that alone admits injection — the specific-symbol path, robust to how many
    /// repos the agent has indexed (IDF shrinks with a small corpus).
    pub strong_specific_score: f32,
    /// Single-repo fallback: with one repo, IDF is uninformative, so admit on a
    /// plain name-match at this strength (preserves pre-multi-repo behaviour).
    pub min_name_match: f32,
    /// A secondary repo contributes only if its score is at least this fraction of
    /// the primary's — keeps assembly focused on the repo(s) the question is about.
    pub secondary_repo_margin: f32,
    /// Weight of a bare lowercase English-word pattern ("capability", "read"): far
    /// below an identifier's, so a generic word — even one that is a rare exact
    /// symbol name — cannot alone clear the gate.
    pub bare_word_shape_weight: f32,
    /// Weight on the primary repo's best semantic score, folded in as a bounded
    /// corroborator (never a sole admit signal — embeddings are noisy).
    pub vector_bonus_weight: f32,
}

impl Default for GroundingConfig {
    fn default() -> Self {
        Self {
            min_grounding_score: 4.0,
            strong_specific_score: 3.5,
            min_name_match: 3.0,
            secondary_repo_margin: 0.75,
            bare_word_shape_weight: 0.35,
            vector_bonus_weight: 1.5,
        }
    }
}

impl GroundingConfig {
    /// The live grounding thresholds resolved from `CodeContextConfig` (per-agent
    /// override → global, hot-swappable).
    pub fn from_config(cfg: &workforce_common::settings::CodeContextConfig) -> Self {
        Self {
            min_grounding_score: cfg.min_grounding_score,
            strong_specific_score: cfg.strong_specific_score,
            min_name_match: cfg.min_name_match,
            secondary_repo_margin: cfg.secondary_repo_margin,
            bare_word_shape_weight: cfg.bare_word_shape_weight,
            vector_bonus_weight: cfg.vector_bonus_weight,
        }
    }
}

/// Whether a token looks like a deliberate code reference rather than an English
/// word: snake_case, `camelCase`/`PascalCase` (mixed case), a digit, or a
/// `::`/`.`/`/` path. Mixed-case (not merely any capital) so an all-caps
/// sentence-start word isn't mistaken for an identifier.
fn is_identifier_shaped(token: &str) -> bool {
    token.contains('_')
        || token.contains("::")
        || token.contains('.')
        || token.contains('/')
        || token.chars().any(|c| c.is_ascii_digit())
        || (token.chars().any(|c| c.is_uppercase()) && token.chars().any(|c| c.is_lowercase()))
}

/// Whether a token is a deliberate CODE-SYMBOL reference, for the injection
/// intent gate ([`question_seeks_code`]). Deliberately STRICTER than
/// [`is_identifier_shaped`]: snake_case, a `::` path, or an internal camelCase
/// hump (a lowercase char immediately followed by an uppercase one) — but NOT a
/// bare digit or a `.`/`/` path. That looser shape is right for the scoring
/// `shape_weight`, but here it would let an ops question read as code — `S3`,
/// `main.csv`, `v2`, a date — and defeat the whole gate.
fn references_code_symbol(token: &str) -> bool {
    token.contains('_')
        || token.contains("::")
        || token
            .chars()
            .zip(token.chars().skip(1))
            .any(|(prev, next)| prev.is_ascii_lowercase() && next.is_ascii_uppercase())
}

/// Whether `question` actually seeks a code answer — the injection intent gate.
/// True iff the question carries a strong code-intent phrase OR names a code
/// symbol. Biased to abstain: a false negative costs only an on-demand grep (the
/// agent's always-available fallback), whereas a false positive is the
/// irrelevant code dump this gate exists to stop. Data/ops/chat questions that
/// merely collide with symbol names ("look up X in the S3 bucket", "did the cost
/// centre sync run") return false. A pre-filter in front of the relevance
/// [`decide`] gate, not a replacement for it.
pub fn question_seeks_code(question: &str) -> bool {
    const CODE_INTENT_PHRASES: [&str; 16] = [
        "how does",
        "what calls",
        "who calls",
        "callers of",
        "callees of",
        "call graph",
        "defined in",
        "declared in",
        "implemented in",
        "implementation of",
        "under the hood",
        "responsible for",
        "what happens when",
        "where in the code",
        // Mechanism COLLOCATIONS, not bare stems: "policy" alone is shared
        // vocabulary in this domain (leave policy, escalation policy) and a
        // bare `polic` stem would inject code into HR questions. The
        // two-word forms are unambiguous. Added from a measured live miss:
        // "Is there any way a tool can run without policy evaluation?" was
        // classified not_code_question (2026-07-31).
        "policy evaluation",
        "policy engine",
    ];
    // Verbs for how the SYSTEM behaves. Deliberately mechanical — `enforce`,
    // `dispatch`, `bypass` — and deliberately not the CRUD/ops vocabulary
    // (`create`, `run`, `sync`, `download`) that a data or operations question
    // shares. Those are what the gate is for.
    //
    // Stems, not whole words: `authorise`/`authorises`/`authorisation` are one
    // idea, and listing inflections is how a rule like this rots. Each is chosen
    // to be unambiguous at the prefix — `rout` is spelled out instead, because it
    // would otherwise swallow "routine".
    const MECHANISM_STEMS: [&str; 22] = [
        "enforc",
        "validat",
        "guard",
        "bypass",
        "invok",
        "handl",
        "parse",
        "parsed",
        "parsing",
        "route",
        "routed",
        "routes",
        "routing",
        "dispatch",
        "implement",
        "configur",
        "trigger",
        "resolv",
        "authoris",
        "authoriz",
        "authenticat",
        "instantiat",
    ];
    const INTERROGATIVES: [&str; 7] = [
        "where",
        "how",
        "which",
        "why",
        "what",
        "is there",
        "are there",
    ];

    let lower = question.to_lowercase();
    // Word-boundary matching for BOTH phrase lists. Bare `contains` let any
    // word that merely embeds a trigger fire the gate — "the SHOW DOESn't
    // start" contains "how does", "whatsapp" contains "what" — and each false
    // pass costs a thousand-line injection of irrelevant code.
    if CODE_INTENT_PHRASES
        .iter()
        .any(|p| contains_word_bounded(&lower, p))
    {
        return true;
    }
    if question.split_whitespace().any(references_code_symbol) {
        return true;
    }
    // "Where do we enforce the allowlist?" names no symbol and matches no
    // call-graph phrase, yet is exactly what this feature is for. Asking how the
    // system behaves, in the vocabulary of mechanism, is code intent.
    let words: Vec<&str> = lower.split_whitespace().collect();
    let asks = INTERROGATIVES
        .iter()
        .any(|q| contains_word_bounded(&lower, q));
    let mechanism = words.iter().any(|w| {
        let bare = w.trim_matches(|c: char| !c.is_alphanumeric());
        MECHANISM_STEMS.iter().any(|stem| bare.starts_with(stem))
    });
    asks && mechanism
}

/// Whether `haystack` contains `needle` starting and ending on word boundaries.
/// `needle` may span several words ("how does"); its interior spaces match
/// literally. Both inputs are expected lowercased.
fn contains_word_bounded(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(needle) {
        let at = from + rel;
        let end = at + needle.len();
        let left_ok = at == 0
            || !haystack[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric());
        let right_ok = end == haystack.len()
            || !haystack[end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric());
        if left_ok && right_ok {
            return true;
        }
        from = at + 1;
    }
    false
}

/// The identifier-shaped tokens of the ORIGINAL question, lowercased. Seed
/// patterns lose their case (content tokens are lowercased in the planner), so a
/// leading-lowercase `camelCase` reference like `undoSuspendAccount` would read as
/// a bare word; this set recovers the shape from the source text.
fn identifier_tokens(question: &str) -> std::collections::HashSet<String> {
    question
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '/'))
                .collect::<String>()
        })
        .filter(|t| is_identifier_shaped(t))
        .map(|t| t.to_lowercase())
        .collect()
}

/// The specificity weight of a question `pattern`. Identifier-shaped tokens
/// (deliberate code references) are trusted at full weight; a bare lowercase
/// English word is weighted far lower, so a conversational word that merely
/// collides with a symbol name cannot alone justify an injection. `idents` covers
/// the `camelCase` references whose case the pattern list has already dropped.
fn shape_weight(
    pattern: &str,
    idents: &std::collections::HashSet<String>,
    cfg: &GroundingConfig,
) -> f32 {
    if is_identifier_shaped(pattern) || idents.contains(&pattern.to_lowercase()) {
        IDENTIFIER_SHAPE_WEIGHT
    } else {
        cfg.bare_word_shape_weight
    }
}

/// Inverse document frequency of a pattern across the searched repos: a token in
/// every repo (`read`, `errors`) tends to 0; one in a single repo
/// (`undoSuspendAccount`) is high. `0` when only one repo was searched — IDF is
/// uninformative there, and the single-repo path gates on name-match instead.
fn idf(df: u16, repo_count: usize) -> f32 {
    if repo_count <= 1 {
        return 0.0;
    }
    (((repo_count as f32) + 1.0) / ((df as f32) + 1.0))
        .ln()
        .max(0.0)
}

/// One repo's aggregate grounding signals over the resolved seeds.
struct RepoRelevance {
    repo_id: String,
    /// Specificity-weighted evidence: Σ over distinct matched patterns of
    /// `strength × shape × idf`, plus a bounded semantic bonus.
    score: f32,
    /// Best single `strength × shape` (IDF-independent) — one identifier-shaped
    /// strong match is a genuine reference regardless of corpus size.
    best_specific: f32,
    /// Best raw name-match strength in this repo (the single-repo fallback gate).
    best_name_match: f32,
    best_vector: f32,
}

/// The grounding decision: inject from these repos (primary first), or skip with
/// a reason for telemetry.
enum GroundingDecision {
    Inject { repos: Vec<String> },
    Skip { reason: &'static str },
}

/// Aggregate the resolved seeds into per-repo relevance, ranked by
/// specificity-weighted score (ties broken by `repo_id` for determinism).
fn rank_repos(
    res: &SeedResolution,
    patterns: &[String],
    idents: &std::collections::HashSet<String>,
    cfg: &GroundingConfig,
) -> Vec<RepoRelevance> {
    // Per repo: the best match strength for each distinct pattern, and the best
    // raw name-match and semantic score.
    struct Acc {
        best_strength: std::collections::HashMap<usize, f32>,
        best_name_match: f32,
        best_vector: f32,
    }
    let mut by_repo: std::collections::HashMap<String, Acc> = std::collections::HashMap::new();
    for seed in &res.seeds {
        let acc = by_repo
            .entry(seed.entity.repo_id.clone())
            .or_insert_with(|| Acc {
                best_strength: std::collections::HashMap::new(),
                best_name_match: 0.0,
                best_vector: 0.0,
            });
        acc.best_name_match = acc.best_name_match.max(seed.name_match);
        acc.best_vector = acc.best_vector.max(seed.vector);
        for hit in &seed.pattern_hits {
            let slot = acc.best_strength.entry(hit.pattern_idx).or_insert(0.0);
            *slot = slot.max(hit.strength);
        }
    }
    let mut ranked: Vec<RepoRelevance> = by_repo
        .into_iter()
        .map(|(repo_id, acc)| {
            let Acc {
                best_strength,
                best_name_match,
                best_vector,
            } = acc;
            // Deterministic fold order (a HashMap iterates arbitrarily) so equal
            // scores are truly equal run-to-run.
            let mut hits: Vec<(usize, f32)> = best_strength.into_iter().collect();
            hits.sort_by_key(|(idx, _)| *idx);
            let (score, best_specific) = hits.iter().fold(
                (0.0_f32, 0.0_f32),
                |(score, best_specific), &(idx, strength)| {
                    let shape = patterns
                        .get(idx)
                        .map(|p| shape_weight(p, idents, cfg))
                        .unwrap_or(cfg.bare_word_shape_weight);
                    let specific = strength * shape;
                    let df = res.pattern_df.get(idx).copied().unwrap_or(0);
                    let idf = idf(df, res.repo_count);
                    // `best_specific` (the IDF-independent admit) credits only a
                    // RARE match (df < repo_count → idf > 0): a generic identifier
                    // in every repo (`read_file`) must not admit an arbitrary one.
                    let specific_admit = if idf > 0.0 { specific } else { 0.0 };
                    (score + specific * idf, best_specific.max(specific_admit))
                },
            );
            RepoRelevance {
                repo_id,
                score: score + cfg.vector_bonus_weight * best_vector,
                best_specific,
                best_name_match,
                best_vector,
            }
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.repo_id.cmp(&b.repo_id))
    });
    ranked
}

/// Decide whether — and from which repos — to ground, from the ranked repos.
/// Pure: the whole inject-or-not policy is unit-testable without I/O. A question
/// injects when the primary repo shows specific/corroborated evidence
/// (`min_grounding_score`), or one identifier-shaped strong match
/// (`strong_specific_score`), or — with a single repo, where IDF is uninformative
/// — a plain name-match (`min_name_match`). Secondary repos join only within the
/// specificity margin, keeping the assembly focused on the right code.
fn decide(ranked: &[RepoRelevance], repo_count: usize, cfg: &GroundingConfig) -> GroundingDecision {
    let Some(primary) = ranked.first() else {
        return GroundingDecision::Skip { reason: "no_seeds" };
    };
    let multi_repo = repo_count > 1;
    let confident = primary.score >= cfg.min_grounding_score
        || primary.best_specific >= cfg.strong_specific_score
        || (!multi_repo && primary.best_name_match >= cfg.min_name_match);
    if !confident {
        return GroundingDecision::Skip {
            reason: "low_confidence",
        };
    }
    // Secondary repos join only when the primary has a positive specificity score
    // to be a fraction of — a `best_specific`/single-repo admit with no IDF signal
    // (`score == 0`) injects the primary alone rather than spraying every repo.
    let cutoff = cfg.secondary_repo_margin * primary.score;
    let repos = ranked
        .iter()
        .filter(|r| r.repo_id == primary.repo_id || (primary.score > 0.0 && r.score >= cutoff))
        .map(|r| r.repo_id.clone())
        .collect();
    GroundingDecision::Inject { repos }
}

/// Fields shared by every `code_context` grounding trace, captured before the
/// resolution is consumed.
#[derive(Clone, Copy)]
struct GroundingTrace {
    question_len: usize,
    seed_count: usize,
    repo_count: usize,
    vector_configured: bool,
}

/// Emit the single `code_context` grounding event — the operational signal for
/// tuning the thresholds: whether it injected, why, and the primary repo's
/// scores.
fn log_grounding(
    injected: bool,
    reason: &str,
    trace: GroundingTrace,
    primary: Option<&RepoRelevance>,
    qualified_repos: usize,
    components: usize,
    lines: usize,
) {
    tracing::info!(
        target: "code_context",
        injected,
        reason,
        question_len = trace.question_len,
        seed_count = trace.seed_count,
        repo_count = trace.repo_count,
        vector_configured = trace.vector_configured,
        primary_repo = primary.map(|p| p.repo_id.as_str()).unwrap_or(""),
        primary_score = primary.map(|p| p.score).unwrap_or(0.0),
        best_name_match = primary.map(|p| p.best_name_match).unwrap_or(0.0),
        best_vector = primary.map(|p| p.best_vector).unwrap_or(0.0),
        qualified_repos,
        components,
        lines,
        "context"
    );
}

/// Default line budget for the answer-ready context block injected into the live
/// answer path (before the model's first turn). Tighter than the standalone
/// tool's [`DEFAULT_BUDGET_LINES`]: it shares the model's context window with the
/// user's message and the conversation history, so it leans toward "enough to
/// answer" over "everything related".
pub const INJECTION_BUDGET_LINES: usize = 1200;

/// Hard ceiling on the injected block's line count, applied AFTER assembly as a
/// defensive backstop so a pathological assembly (many tiny components, a huge
/// single body) can never blow the context window. The assembler already bounds
/// output by `budget_lines`; this only ever bites a runaway and truncates with a
/// visible marker rather than erroring.
const INJECTION_MAX_LINES: usize = 2000;

/// Header that labels the injected block as authoritative current source the
/// model must answer from. Kept as a const so the test that proves the block
/// reaches the model's first turn asserts against the same string the runtime
/// emits.
///
/// The contract is deliberately asymmetric — answer-first, tools for gaps only.
/// The previous wording invited tool use in BOTH branches ("use your tools to go
/// deeper" / "use your tools to find the right code"), and a production trace
/// showed the cost: a model handed 1,359 relevant lines re-investigated the
/// codebase for 17 rounds (277K tokens of tool output) and hit its round cap,
/// because a full re-derivation was compliant with the header as written. Three
/// obligations replace it:
///
/// 1. Answer FROM this block, citing file:line — no tool call needed for what it
///    already shows.
/// 2. Tools only for what the block does not contain — never to re-read or
///    re-find what is already here.
/// 3. A claim that something does NOT exist is never settled by this block (it
///    is a selected subset): search before asserting absence.
///
/// The escape hatch (block doesn't address the question → say so and search) is
/// retained verbatim in spirit; the test suite pins hatch and obligations both.
pub const INJECTION_HEADER: &str = "## CURRENT SOURCE CODE — assembled for your question, read from the repository at HEAD. It is current and authoritative. Answer directly FROM this source and cite file:line. Do not re-read or re-search anything shown here — use tools ONLY for what this block does not contain. Because it is a selected subset, it cannot prove absence: before asserting that something does not exist anywhere, search beyond this block. If this source does not actually address the question, say so and search for the right code — do not force an answer from unrelated code:";

/// One-line reminder placed directly BEFORE the user's question, after the
/// assembled block. The header sits at the top and the question can be a
/// thousand lines below it; a model attending to the question last needs the
/// obligation adjacent to it, not a scroll away.
pub const INJECTION_ANSWER_FIRST: &str = "Answer the question below from the CURRENT SOURCE CODE block above; call tools only for what it lacks.";

/// Maximum number of [`CodeCitation`]s surfaced under an answer. The assembled
/// block can pull in a function plus its whole call-graph closure; capping the
/// citation list keeps the UI's "Sources" row legible (the directly-relevant
/// entities lead, ordered Seed-first) without listing every supplementary body.
const MAX_CITATIONS: usize = 12;

/// A ready-to-read bundle of source: the formatted `text`, plus provenance
/// (`repo` + `commit`) and how many distinct components it contains.
#[derive(Debug, Clone)]
pub struct AssembledContext {
    /// The assembled, labelled source block — a one-line header followed by
    /// each component's locator comment and body.
    pub text: String,
    /// Repo name the source came from.
    pub repo: String,
    /// Short HEAD commit the bodies were read at (empty when git is
    /// unavailable — provenance is best-effort, never required).
    pub commit: String,
    /// Number of distinct components included after dedup + budget.
    pub components: usize,
    /// Deterministic, clickable citations for the directly-relevant grounded
    /// entities (seeds + their file/type context). Independent of whatever
    /// `file:line` prose the model writes; the UI renders these as GitHub
    /// permalink chips under the answer. Filtered to `reached_via` ∈
    /// {Seed, FileContext, Type}, deduped by (path, start_line), capped at
    /// [`MAX_CITATIONS`], Seed-first.
    pub citations: Vec<CodeCitation>,
}

/// The assembled source as a clearly-labelled, authoritative block, ending with
/// the answer-first reminder so the obligation sits adjacent to the question
/// that follows it. Pure (no I/O): this is the seam the live answer path
/// injects through and the unit tests assert against.
///
/// Deliberately does NOT take the user's message. The block is system-produced
/// (read from the repository at HEAD) while the message is sender-supplied;
/// composing them here forced the caller to trust-wrap them as one, which
/// labelled the system's own source as untrusted user content for paired
/// senders. The caller (`build_task_content`) now owns composition and wraps
/// only the message.
///
/// The assembled text is line-capped at [`INJECTION_MAX_LINES`] as a defensive
/// backstop — never erroring, only truncating a runaway with a visible marker —
/// so an absurdly large block can't blow the model's context window.
pub fn build_injected_block(context: &str) -> String {
    let lines: Vec<&str> = context.lines().collect();
    let capped = if lines.len() > INJECTION_MAX_LINES {
        let mut kept: String = lines[..INJECTION_MAX_LINES].join("\n");
        kept.push_str(&format!(
            "\n// … assembled context truncated at {INJECTION_MAX_LINES} lines (over budget) …"
        ));
        kept
    } else {
        context.to_string()
    };
    format!("{INJECTION_HEADER}\n{capped}\n\n{INJECTION_ANSWER_FIRST}")
}

/// Render the deterministic grounding line appended to the tail of an answer:
/// the assembler's own citations, so the reader sees exactly which current
/// files and lines the answer rests on. This is the SYSTEM's record of what it
/// injected — independent of any `file:line` prose the model wrote — and is the
/// visible confirmation on surfaces without citation chips (Slack).
///
/// Compact by design: at most [`GROUNDING_LINE_MAX`] locations, "+N more"
/// beyond, one commit stamp (the citations share the primary repo's HEAD).
pub fn grounding_line(citations: &[CodeCitation]) -> String {
    const GROUNDING_LINE_MAX: usize = 8;
    if citations.is_empty() {
        return String::new();
    }
    let shown: Vec<String> = citations
        .iter()
        .take(GROUNDING_LINE_MAX)
        .map(|c| format!("`{}:{}`", c.path, c.start_line))
        .collect();
    let more = citations.len().saturating_sub(GROUNDING_LINE_MAX);
    let more_note = if more > 0 {
        format!(" +{more} more")
    } else {
        String::new()
    };
    let commit = citations
        .first()
        .map(|c| c.commit.as_str())
        .filter(|c| !c.is_empty())
        .map(|c| format!(" @ {}", &c[..c.len().min(9)]))
        .unwrap_or_default();
    format!(
        "\n\n_Grounded in {}{}{}_",
        shown.join(", "),
        more_note,
        commit
    )
}

/// Hub service that assembles answer-ready code context. Holds the knowledge
/// service (cross-repo seed resolution + graph closure), the agent workspace
/// (the fallback checkout when a `repositories` row carries no on-disk
/// `root_path` — single-agent setups whose workspace IS the indexed repo), and
/// — in the multi-agent hub — the maintenance runner + agent id used to SCOPE
/// the cross-repo search to THIS agent's repos.
///
/// The codegraph store is SHARED across agents in the hub (one `repositories`
/// table for every agent's repos), so the cross-repo seed space must be
/// restricted to the repos this agent actually owns — otherwise a question on
/// agent A could assemble code from agent B's repos. When the runner is absent
/// (single-agent / tests), the scope falls back to the agent's workspace repo
/// only, which is the conservative pre-multi-repo behaviour.
pub struct CodeContextService {
    service: Arc<KnowledgeService>,
    agent_workspace: PathBuf,
    scope: RepoScope,
    grounding: GroundingConfig,
}

/// How the cross-repo seed space is restricted to the calling agent's repos.
/// Always includes the agent's workspace repo; the variants add the rest.
enum RepoScope {
    /// No agent association available (single-agent / tests) → workspace repo
    /// only. Conservative: under-scope, never cross-agent over-scope.
    WorkspaceOnly,
    /// The multi-agent hub: enumerate the agent's targets from the runner
    /// (`agent_ids` filter) at assemble-time, since the set changes as repos are
    /// (un)registered and the runner is wired after construction.
    Agent {
        runner: Arc<MaintenanceRunner>,
        agent_id: String,
    },
    /// An explicit repo_id allowlist — used by tests to exercise the scoped path
    /// without standing up a real `MaintenanceRunner`.
    #[cfg(test)]
    Explicit(std::collections::HashSet<String>),
}

impl CodeContextService {
    pub fn new(service: Arc<KnowledgeService>, agent_workspace: PathBuf) -> Self {
        Self {
            service,
            agent_workspace,
            scope: RepoScope::WorkspaceOnly,
            grounding: GroundingConfig::default(),
        }
    }

    /// Scope cross-repo resolution to the repos this `agent_id` owns, enumerated
    /// via the `runner` (filtered by `agent_ids`) — required in the multi-agent
    /// hub where the codegraph store is shared across agents.
    pub fn with_agent_scope(mut self, runner: Arc<MaintenanceRunner>, agent_id: String) -> Self {
        self.scope = RepoScope::Agent { runner, agent_id };
        self
    }

    /// Override the grounding thresholds (the inject-or-not / which-repo policy)
    /// from live config, mirroring [`Self::with_agent_scope`].
    pub fn with_grounding(mut self, grounding: GroundingConfig) -> Self {
        self.grounding = grounding;
        self
    }

    /// Scope to an explicit repo_id allowlist — test-only, to exercise the
    /// scoped multi-repo path without a real `MaintenanceRunner`.
    #[cfg(test)]
    fn with_repo_scope(mut self, repo_ids: impl IntoIterator<Item = String>) -> Self {
        self.scope = RepoScope::Explicit(repo_ids.into_iter().collect());
        self
    }

    /// Assemble the complete relevant source for `question`, bounded by
    /// `budget_lines`. Returns `None` when no seed entities resolve (the symbol
    /// graph has nothing for the question) — the caller should then fall back
    /// to grep/prose search.
    ///
    /// **Multi-repo**: an agent may have several repos indexed (the live
    /// driftwerk agent indexes `driftwerk/workforce`, `driftwerk/orbit`,
    /// `driftwerk/infra`, …) and the relevant code can live in any of them. Seed
    /// resolution runs across ALL the agent's indexed repos at once (one graph
    /// store keyed by `repo_id`), ranks every candidate with the same relevance
    /// score, and assembles context from whichever repos the winning seeds belong
    /// to — reading each component's body from that repo's own checkout. A
    /// single-repo agent resolves over its one repo, identical to before.
    pub async fn assemble_for(
        &self,
        question: &str,
        budget_lines: usize,
    ) -> Option<AssembledContext> {
        // Enumerate every repo the agent has indexed, each with its on-disk
        // checkout (`root_path`). The graph is one store keyed by `repo_id`, so
        // this single enumeration is the cross-repo seed space — no per-repo
        // store to fan out over. A resolver-less single-repo agent yields exactly
        // one row, preserving prior behaviour.
        let repos = self.indexed_repos().await;
        if repos.is_empty() {
            log_grounding(
                false,
                "no_indexed_repos",
                GroundingTrace {
                    question_len: question.len(),
                    seed_count: 0,
                    repo_count: 0,
                    vector_configured: false,
                },
                None,
                0,
                0,
                0,
            );
            return None;
        }
        let repo_ids: Vec<String> = repos.iter().map(|r| r.repo_id.clone()).collect();

        // (a) Seed entities across ALL indexed repos via the SAME retrieval
        //     `knowledge.ask` performs, plus the per-pattern document frequency
        //     the grounding decision weighs specificity with.
        let plan = QueryPlanner::plan(question, false);
        let resolution = self
            .service
            .query_engine()
            .resolve_seed_entities_multi(&repo_ids, question, &plan)
            .await
            .ok()?;
        let trace = GroundingTrace {
            question_len: question.len(),
            seed_count: resolution.seeds.len(),
            repo_count: resolution.repo_count,
            vector_configured: resolution.vector_configured,
        };

        // (b) Decide whether — and from which repo(s) — to ground. A conversational
        //     question that only brushes a symbol name, or whose only matches are
        //     generic English words colliding with symbols, resolves to Skip: the
        //     agent then greps on demand rather than getting the wrong code dumped
        //     in. This is what keeps the hub's own source out of a product question.
        let idents = identifier_tokens(question);
        let ranked = rank_repos(&resolution, &plan.entity_patterns, &idents, &self.grounding);
        let chosen = match decide(&ranked, resolution.repo_count, &self.grounding) {
            GroundingDecision::Skip { reason } => {
                log_grounding(false, reason, trace, ranked.first(), 0, 0, 0);
                return None;
            }
            GroundingDecision::Inject { repos } => repos,
        };

        self.assemble_chosen(
            &repos,
            chosen,
            resolution.seeds,
            &plan,
            trace,
            ranked.first(),
            budget_lines,
            "inject",
        )
        .await
    }

    /// Assemble scoped to ONE explicitly named repo — the `code.context`
    /// tool's `repo` argument. The agent's repo choice replaces the
    /// cross-repo rank/decide gate: that gate exists to approximate exactly
    /// this judgment, and the caller has supplied it, so a pinned repo
    /// assembles whatever relevant seeds it holds (still test-filtered,
    /// relevance-sorted, and budget-bounded). `Err` is the user-facing
    /// selector-resolution message; `Ok(None)` means the repo holds no
    /// seeds for the question. Resolution stays inside `indexed_repos()`,
    /// so a selector can never reach another agent's repo.
    pub async fn assemble_in_repo(
        &self,
        question: &str,
        selector: &str,
        budget_lines: usize,
    ) -> Result<Option<AssembledContext>, String> {
        let repos = self.indexed_repos().await;
        let repo_id = resolve_repo_selector(&repos, selector)?.repo_id.clone();

        let plan = QueryPlanner::plan(question, false);
        let resolution = self
            .service
            .query_engine()
            .resolve_seed_entities_multi(std::slice::from_ref(&repo_id), question, &plan)
            .await
            .map_err(|e| format!("seed resolution failed: {e}"))?;
        let trace = GroundingTrace {
            question_len: question.len(),
            seed_count: resolution.seeds.len(),
            repo_count: resolution.repo_count,
            vector_configured: resolution.vector_configured,
        };
        if resolution.seeds.is_empty() {
            log_grounding(false, "no_seeds_in_repo", trace, None, 0, 0, 0);
            return Ok(None);
        }
        Ok(self
            .assemble_chosen(
                &repos,
                vec![repo_id],
                resolution.seeds,
                &plan,
                trace,
                None,
                budget_lines,
                "explicit_repo",
            )
            .await)
    }

    #[allow(clippy::too_many_arguments)]
    async fn assemble_chosen(
        &self,
        repos: &[IndexedRepo],
        chosen: Vec<String>,
        resolution_seeds: Vec<ScoredSeed>,
        plan: &RetrievalPlan,
        trace: GroundingTrace,
        primary_rank: Option<&RepoRelevance>,
        budget_lines: usize,
        inject_reason: &'static str,
    ) -> Option<AssembledContext> {
        // Always seed off the implementation, not the tests — a seed drags its
        // whole file in as file-context, so one test class eats the budget with
        // fixtures. This used to apply only when `plan.is_explanation`, which let
        // an incident-shaped question ("… websocket errors … what can we do
        // about that?") seed straight off a `*Test.kt`. `assemble_repo_blocks`
        // bars them from the supplementary path too, so a test never reaches the
        // prompt by any route; the agent reads one on demand when it wants one.
        // Keep only the chosen repos' seeds, highest-relevance first (the
        // resolution is already sorted), capped at SEED_LIMIT.
        let seeds: Vec<CodeEntity> = resolution_seeds
            .into_iter()
            .filter(|s| chosen.contains(&s.entity.repo_id))
            .map(|s| s.entity)
            .filter(|e| !is_test_entity(e))
            .take(SEED_LIMIT)
            .collect();
        if seeds.is_empty() {
            log_grounding(
                false,
                "empty_after_filter",
                trace,
                primary_rank,
                chosen.len(),
                0,
                0,
            );
            return None;
        }

        // Group the seeds by repo, preserving rank order within each group. The
        // decided primary repo leads (its checkout stamps the top-level provenance).
        let terms = salient_terms(&plan.entity_patterns);
        let primary_repo_id = chosen[0].clone();
        let mut by_repo: Vec<(String, Vec<EntityId>)> = Vec::new();
        for seed in &seeds {
            match by_repo.iter_mut().find(|(rid, _)| *rid == seed.repo_id) {
                Some((_, ids)) => ids.push(seed.id.clone()),
                None => by_repo.push((seed.repo_id.clone(), vec![seed.id.clone()])),
            }
        }
        // Primary repo first, so its components (and provenance) lead.
        by_repo.sort_by_key(|(rid, _)| *rid != primary_repo_id);

        // (b–e) Assemble each contributing repo's blocks from its own checkout,
        //       sharing one global line budget and one identical-body dedup set
        //       across repos. The first repo gets the full budget; later repos
        //       take what's left.
        let mut seen_bodies: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut blocks: Vec<String> = Vec::new();
        // Each citation carries its `reached_via` ordering rank (lower = more
        // directly relevant) so the final list across repos can be ordered
        // Seed-first; the rank is dropped before assembling the public list.
        let mut ranked_citations: Vec<(u8, CodeCitation)> = Vec::new();
        // The provenance header and its joiner are emitted lines too.
        let mut budget_left = budget_lines.saturating_sub(2);
        let mut primary_repo_name = String::new();
        let mut primary_commit = String::new();
        for (repo_id, seed_ids) in &by_repo {
            let Some(repo) = repos.iter().find(|r| &r.repo_id == repo_id) else {
                continue;
            };
            let assembly = self
                .assemble_repo_blocks(repo, seed_ids, &terms, plan, budget_left, &mut seen_bodies)
                .await;
            if repo.repo_id == primary_repo_id {
                primary_repo_name = repo.name.clone();
                primary_commit = repo.commit.clone();
            }
            // Charge the produced bodies against the shared budget so a 3-repo
            // agent can't return 3× the requested lines.
            for (block, span) in assembly.blocks {
                budget_left = budget_left.saturating_sub(span);
                blocks.push(block);
            }
            ranked_citations.extend(assembly.citations);
        }
        if blocks.is_empty() {
            log_grounding(
                false,
                "empty_assembly",
                trace,
                primary_rank,
                chosen.len(),
                0,
                0,
            );
            return None;
        }
        // Order Seed-first across all contributing repos (a STABLE sort, so within
        // a rank the global seed/graph order from `full_context_for` is kept),
        // dedupe by (path, start_line) across repos, then cap. The bodies were
        // already deduped by content; this is the citation list shown under the
        // answer.
        ranked_citations.sort_by_key(|(rank, _)| *rank);
        let mut seen_locs: std::collections::HashSet<(String, usize)> =
            std::collections::HashSet::new();
        let citations: Vec<CodeCitation> = ranked_citations
            .into_iter()
            .filter(|(_, c)| seen_locs.insert((c.path.clone(), c.start_line)))
            .map(|(_, c)| c)
            .take(MAX_CITATIONS)
            .collect();
        // Fall back to the first contributing repo's identity if the primary repo
        // produced no readable bodies (e.g. its checkout moved).
        if primary_repo_name.is_empty() {
            let first_contributing = &by_repo[0].0;
            if let Some(first) = repos.iter().find(|r| &r.repo_id == first_contributing) {
                primary_repo_name = first.name.clone();
                primary_commit = first.commit.clone();
            }
        }

        let components = blocks.len();
        let multi = by_repo.len() > 1;
        let commit_label = if primary_commit.is_empty() {
            "uncommitted".to_string()
        } else {
            primary_commit.clone()
        };
        let repo_label = if multi {
            let names: Vec<&str> = by_repo
                .iter()
                .filter_map(|(rid, _)| repos.iter().find(|r| &r.repo_id == rid))
                .map(|r| r.name.as_str())
                .collect();
            names.join(", ")
        } else {
            primary_repo_name.clone()
        };
        let header = format!(
            "// {components} component{} from {repo_label} @ {commit_label}",
            if components == 1 { "" } else { "s" }
        );
        let text = std::iter::once(header)
            .chain(blocks)
            .collect::<Vec<_>>()
            .join("\n\n");

        log_grounding(
            true,
            inject_reason,
            trace,
            primary_rank,
            chosen.len(),
            components,
            text.lines().count(),
        );

        Some(AssembledContext {
            text,
            repo: primary_repo_name,
            commit: primary_commit,
            components,
            citations,
        })
    }

    /// Every repo THIS AGENT has indexed, each resolved to its checkout dir + the
    /// HEAD commit the bodies will be read at. The graph's `repositories` rows
    /// supply `root_path`/commit/name (resolver-free); the agent SCOPE — the set
    /// of repo_ids this agent owns — restricts which rows are eligible so a shared
    /// store never leaks another agent's code. A row whose checkout is
    /// missing/unreadable is dropped. Single-repo agents yield one entry.
    async fn indexed_repos(&self) -> Vec<IndexedRepo> {
        let rows = match self.service.graph().list_repositories().await {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        let workspace_repo_id = KnowledgeService::repo_id_for(&self.agent_workspace);
        // Allowed repo_id set for this agent, plus the GitHub owner/repo for each
        // (the citation permalink coordinates). With a maintenance runner (the
        // multi-agent hub), the scope is the agent's `enumerate_targets()` repos
        // and the GitHub map is populated from their `RepoSource`; without one
        // (single-agent / tests), the scope is just the workspace repo and the
        // map is empty — citations then carry an empty owner and the UI falls
        // back to non-link text.
        let scope = self.agent_repo_scope(&workspace_repo_id).await;
        let mut out: Vec<IndexedRepo> = Vec::new();
        for row in rows {
            if !scope.allowed.contains(&row.id) {
                continue; // another agent's repo (or not this agent's) — skip
            }
            let (owner, repo_name) = scope.github.get(&row.id).cloned().unwrap_or_default();
            // Canonicalize for the containment-checked reader (macOS
            // /var→/private/var symlinks etc.). Fall back to the agent workspace
            // when the stored checkout is missing but this is the workspace repo;
            // otherwise skip a row whose checkout no longer exists on disk.
            let dir = match std::fs::canonicalize(&row.root_path) {
                Ok(dir) => dir,
                Err(_) if row.id == workspace_repo_id => {
                    match std::fs::canonicalize(&self.agent_workspace) {
                        Ok(dir) => dir,
                        Err(_) => continue,
                    }
                }
                Err(_) => continue,
            };
            let commit = head_commit(&dir).await.unwrap_or_default();
            out.push(IndexedRepo {
                repo_id: row.id,
                name: row.name,
                checkout_dir: dir,
                commit,
                owner,
                repo_name,
            });
        }
        out
    }

    /// The set of repo_ids this agent is allowed to resolve seeds from, plus the
    /// GitHub owner/repo for each (the citation permalink coordinates). In the
    /// multi-agent hub the codegraph store is shared, so the scope MUST come from
    /// the agent's own targets (`enumerate_targets()` filtered by `agent_ids`),
    /// mapped to repo_ids the same way the indexer derives them
    /// (`KnowledgeService::repo_id_for(&target.dest)`). Without a runner
    /// (single-agent / tests) the scope is the agent's workspace repo only — the
    /// conservative pre-multi-repo behaviour, never the whole shared store — and
    /// the GitHub map is empty (the workspace repo carries no `RepoSource` here),
    /// so citations fall back to non-link text.
    /// `enumerate_targets` does blocking directory + DB reads, so it runs off the
    /// async runtime (matching `agent_repo_structure_map`).
    async fn agent_repo_scope(&self, workspace_repo_id: &str) -> RepoScopeResolution {
        match &self.scope {
            RepoScope::WorkspaceOnly => RepoScopeResolution::workspace_only(workspace_repo_id),
            #[cfg(test)]
            RepoScope::Explicit(set) => {
                let mut resolution = RepoScopeResolution::workspace_only(workspace_repo_id);
                resolution.allowed.extend(set.iter().cloned());
                resolution
            }
            RepoScope::Agent { runner, agent_id } => {
                let runner = Arc::clone(runner);
                let agent_id = agent_id.clone();
                let workspace_repo_id = workspace_repo_id.to_string();
                let fallback = workspace_repo_id.clone();
                tokio::task::spawn_blocking(move || {
                    let mut resolution = RepoScopeResolution::workspace_only(&workspace_repo_id);
                    let targets = match runner.enumerate_targets() {
                        Ok(targets) => targets,
                        // Enumeration failed (transient store/dir hiccup): fail
                        // CLOSED to the workspace repo, never open to the store.
                        Err(_) => return resolution,
                    };
                    for target in targets
                        .into_iter()
                        .filter(|t| t.agent_ids.iter().any(|id| id == &agent_id))
                    {
                        let repo_id = KnowledgeService::repo_id_for(&target.dest);
                        let RepoSource::Github { owner, repo } = &target.source;
                        resolution
                            .github
                            .insert(repo_id.clone(), (owner.clone(), repo.clone()));
                        resolution.allowed.insert(repo_id);
                    }
                    resolution
                })
                .await
                .unwrap_or_else(|_| RepoScopeResolution::workspace_only(&fallback))
            }
        }
    }

    /// Build the labelled component blocks for ONE repo: expand the seeds to the
    /// budget-bounded closure, read each body from this repo's checkout, dedup
    /// identical bodies against the shared `seen_bodies` set, and append the grep
    /// fallback for prose the symbol graph can't see. Returns each block with its
    /// line span (so the caller can charge it against the shared budget) and a
    /// citation per directly-relevant included entity (so the answer's grounding
    /// is surfaced as clickable chips).
    async fn assemble_repo_blocks(
        &self,
        repo: &IndexedRepo,
        seed_ids: &[EntityId],
        terms: &[String],
        plan: &workforce_codebase_knowledge::query::natural_language::RetrievalPlan,
        budget_left: usize,
        seen_bodies: &mut std::collections::HashSet<String>,
    ) -> RepoAssembly {
        if budget_left == 0 {
            return RepoAssembly::default();
        }
        let closure = match self
            .service
            .graph()
            .full_context_for(&repo.repo_id, seed_ids, terms, budget_left)
            .await
        {
            Ok(c) if !c.is_empty() => c,
            _ => return RepoAssembly::default(),
        };

        let reader = ReadFile::new(vec![repo.checkout_dir.clone()]);
        let mut blocks: Vec<(String, usize)> = Vec::new();
        let mut body_blocks: Vec<String> = Vec::new();
        let mut citations: Vec<(u8, CodeCitation)> = Vec::new();
        // Honest accounting: a block's cost is what it actually adds to the
        // emitted text — locator line and joiner included — not the entity's
        // body span. The old body-only charge is how a "1200-line budget"
        // reached 1,359 emitted lines (one locator + one joiner per component,
        // all uncounted). The closure is priority-ordered (seeds → file-context
        // → supplementary), so greedy skip-if-over drops the most supplementary
        // blocks first, same semantics as the closure's own budget pass.
        let mut spent: usize = 0;
        for ce in &closure {
            // Barring tests as SEEDS is not enough: a test calls the code it
            // tests, so the graph reaches it as a `caller` and it lands as a
            // supplementary block anyway — verbatim fixtures against the same
            // 1200-line budget the implementation is competing for. `citation_rank`
            // already treats a test as too noisy to cite; this stops it being
            // assembled at all. The agent can still read the test on demand.
            if is_test_entity(&ce.entity) {
                continue;
            }
            let Some(body) = read_entity_body(&reader, ce).await else {
                continue;
            };
            if !seen_bodies.insert(body.clone()) {
                continue; // identical body already included (this or another repo)
            }
            let block = format_component(&repo.name, ce, &body);
            let span = block.lines().count() + 1; // +1: the "\n\n" joiner's blank line
            if spent + span > budget_left && !blocks.is_empty() {
                continue; // over budget — later (smaller) blocks may still fit
            }
            spent += span;
            body_blocks.push(block.clone());
            blocks.push((block, span));
            // Only the directly-relevant entities are cited — the seed, its
            // same-file context, and the types in its signature. Callees/callers
            // are supplementary and noisy, so they're assembled but not cited.
            if let Some(rank) = citation_rank(ce.reached_via) {
                citations.push((rank, citation_for(repo, ce)));
            }
        }
        if blocks.is_empty() {
            return RepoAssembly::default();
        }

        // Grep fallback: the symbol graph can't represent module-level
        // doc-comments (`//!` on-disk-format notes), bare consts, or prose between
        // definitions. Grep this repo's seed files for the question's salient
        // terms and surface matching lines the bodies didn't already include.
        if let Some(grep_block) = grep_seed_files(
            &repo.checkout_dir,
            &repo.name,
            &closure,
            &plan.entity_patterns,
            &body_blocks,
        )
        .await
        {
            // The grep block is bounded (GREP_FALLBACK_MAX_LINES) and charged
            // honestly like every other block — joiner included — and only if it
            // still fits.
            let span = grep_block.lines().count() + 1;
            // NOTE for the next block type added below this one: `spent` is NOT
            // incremented here because grep is currently the LAST admission —
            // anything appended after it must resume the accounting or it will
            // slip under the budget.
            if spent + span <= budget_left {
                blocks.push((grep_block, span));
            }
        }
        RepoAssembly { blocks, citations }
    }
}

/// One repo's contribution to an assembly: the labelled blocks (each with its
/// line span for budget accounting) and the ranked citations for its included
/// directly-relevant entities (rank = `reached_via` priority, Seed-first).
#[derive(Default)]
struct RepoAssembly {
    blocks: Vec<(String, usize)>,
    citations: Vec<(u8, CodeCitation)>,
}

/// The Seed-first ordering rank for a cited entity, or `None` if the entity is
/// supplementary (a callee/caller/test/impl) and should NOT be cited. Lower rank
/// = more directly relevant: seeds lead, then their same-file context, then the
/// types in their signatures.
fn citation_rank(reached_via: ReachedVia) -> Option<u8> {
    match reached_via {
        ReachedVia::Seed => Some(0),
        ReachedVia::FileContext => Some(1),
        ReachedVia::Type => Some(2),
        ReachedVia::Callee | ReachedVia::Caller | ReachedVia::Test | ReachedVia::Impl => None,
    }
}

/// Build a [`CodeCitation`] from an included context entity + its repo. The
/// GitHub `owner`/`repo_name` come from the repo's resolved source (empty when
/// not a GitHub target → the UI renders non-link text).
fn citation_for(repo: &IndexedRepo, ce: &ContextEntity) -> CodeCitation {
    let e = &ce.entity;
    CodeCitation {
        repo: repo.name.clone(),
        owner: repo.owner.clone(),
        repo_name: repo.repo_name.clone(),
        commit: repo.commit.clone(),
        path: e.file_path.clone(),
        start_line: e.start_line,
        end_line: e.end_line,
        kind: e.kind.as_str().to_string(),
        symbol: e.qualified_name.clone(),
    }
}

/// One indexed repo resolved to everything the assembler needs to read its
/// bodies AND build citations: the graph `repo_id`, the display name, the
/// on-disk checkout, the HEAD commit the read is stamped with, and the GitHub
/// `owner`/`repo_name` permalink coordinates (empty when the repo's source
/// isn't a resolvable GitHub target — the citation then renders as plain text).
#[derive(Debug)]
struct IndexedRepo {
    repo_id: String,
    name: String,
    checkout_dir: PathBuf,
    commit: String,
    owner: String,
    repo_name: String,
}

/// The name a repo is addressed and listed by in selector errors:
/// `owner/repo` when the GitHub coordinates exist, else the stored name.
fn repo_display(repo: &IndexedRepo) -> String {
    if repo.owner.is_empty() || repo.repo_name.is_empty() {
        repo.name.clone()
    } else {
        format!("{}/{}", repo.owner, repo.repo_name)
    }
}

/// What a pinned `code.context` call is told when the agent has no repo in
/// scope. The hub may well have the repo indexed for another agent, so the
/// message says what is true — nothing is attached here — and names the tools
/// that read indexed repos whoever they are attached to.
const NO_AGENT_REPOS: &str =
    "code.context has nothing to assemble for this agent: no indexed repo \
    is attached to it (an agent's projects are what attach repos). Do not retry it. Repos the hub \
    has indexed are still read directly — `repo.list` names them, `repo.grep` locates a symbol \
    and `repo.read` reads the file.";

/// Resolve a `code.context` `repo` selector against THIS agent's indexed
/// repos, mirroring the repo-catalog UX agents already know from the
/// `repo.*` tools: case-insensitive exact match on the display name or
/// stored name first, then a bare-repo-name match; ambiguity and unknown
/// names return teaching errors listing the valid choices.
fn resolve_repo_selector<'a>(
    repos: &'a [IndexedRepo],
    selector: &str,
) -> Result<&'a IndexedRepo, String> {
    if repos.is_empty() {
        return Err(NO_AGENT_REPOS.to_string());
    }
    let wanted = selector.trim().to_ascii_lowercase();
    let exact: Vec<&IndexedRepo> = repos
        .iter()
        .filter(|r| {
            repo_display(r).to_ascii_lowercase() == wanted || r.name.to_ascii_lowercase() == wanted
        })
        .collect();
    let candidates = if exact.is_empty() {
        repos
            .iter()
            .filter(|r| {
                r.repo_name.to_ascii_lowercase() == wanted
                    || r.name
                        .rsplit('/')
                        .next()
                        .is_some_and(|last| last.to_ascii_lowercase() == wanted)
            })
            .collect()
    } else {
        exact
    };
    let mut names: Vec<String> = candidates.iter().map(|r| repo_display(r)).collect();
    names.sort();
    names.dedup();
    match candidates.as_slice() {
        [only] => Ok(only),
        [] => {
            let mut indexed: Vec<String> = repos.iter().map(repo_display).collect();
            indexed.sort();
            indexed.dedup();
            Err(format!(
                "unknown repo '{selector}'; indexed: {} (pass a repo name from this \
                 list — project names the repo.* tools accept are not valid here)",
                indexed.join(", ")
            ))
        }
        _ => Err(format!(
            "'{selector}' is ambiguous; name one of: {}",
            names.join(", ")
        )),
    }
}

/// The agent's repo scope resolved for an assembly: which repo_ids are in scope
/// and, for the GitHub-sourced ones, their `owner`/`repo` for citation
/// permalinks. The GitHub map is a subset of `allowed` (a repo can be in scope
/// without a GitHub source — e.g. the workspace repo or a non-GitHub target).
struct RepoScopeResolution {
    allowed: std::collections::HashSet<String>,
    github: std::collections::HashMap<String, (String, String)>,
}

impl RepoScopeResolution {
    /// The conservative fallback: only the agent's own workspace repo in scope,
    /// with no GitHub coordinates (so its citations render as non-link text).
    fn workspace_only(workspace_repo_id: &str) -> Self {
        Self {
            allowed: std::iter::once(workspace_repo_id.to_string()).collect(),
            github: std::collections::HashMap::new(),
        }
    }
}

/// Read the body of `ce` from the checkout via the containment-checked reader.
/// `start_line` is 1-indexed; `ReadFile`'s `offset` is 0-indexed, so the offset
/// is `start_line - 1` and the limit is the inclusive span. `None` on any read
/// failure (file moved/renamed since indexing, span past EOF) so one stale
/// entity can't sink the whole assembly.
async fn read_entity_body(reader: &ReadFile, ce: &ContextEntity) -> Option<String> {
    let e = &ce.entity;
    let offset = e.start_line.saturating_sub(1);
    let limit = e.end_line.saturating_sub(e.start_line) + 1;
    let result = reader
        .execute(json!({
            "path": e.file_path,
            "offset": offset,
            "limit": limit,
        }))
        .await
        .ok()?;
    if !result.success {
        return None;
    }
    let content = result.output.get("content")?.as_str()?.to_string();
    if content.trim().is_empty() {
        return None;
    }
    Some(content)
}

/// Whether an entity is a test — a `#[test]`/`#[tokio::test]` function, a
/// `mod tests` definition, or anything living in a test file by the conventions
/// of any language we index. Such entities are excluded as *seeds* so they don't
/// drag whole test files in as file-context.
///
/// The `kind`/`tests::` clauses only ever fired for Rust, so this leant entirely
/// on `file_path.contains("/tests/")` elsewhere — which matches no JVM, Ruby, TS
/// or Go layout at all. [`is_test_path`] is the shared, language-aware
/// replacement; the first two clauses stay so a Rust `mod tests` inside a
/// non-test file is still caught.
fn is_test_entity(e: &CodeEntity) -> bool {
    matches!(e.kind, EntityKind::Test)
        || e.qualified_name.contains("tests::")
        || workforce_codebase_knowledge::graph::entities::is_test_path(&e.file_path)
}

/// The salient question terms used to rank sibling co-pull and seed the grep
/// fallback: alphabetic identifier patterns of length ≥ 3, lower-cased. Drops
/// path/`::` fragments and short noise.
fn salient_terms(patterns: &[String]) -> Vec<String> {
    patterns
        .iter()
        .filter(|p| p.len() >= 3 && p.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .map(|p| p.to_lowercase())
        .collect()
}

/// Maximum grep-fallback lines appended to the assembled context. Keeps the
/// prose supplement bounded — it's a backstop for doc-comments/consts the symbol
/// graph can't see, not a second full search.
const GREP_FALLBACK_MAX_LINES: usize = 40;

/// Grep the **seed files** for the question's salient terms and return a single
/// supplementary block of the matching lines that the assembled symbol bodies
/// didn't already cover — the module-level doc-comments, bare consts, and prose
/// the symbol graph structurally can't represent.
///
/// Scope is deliberately narrow: only files that already hold a seed entity (not
/// the whole repo), only lines containing a salient term, and only lines absent
/// from the bodies already assembled. Returns `None` when there's nothing to add
/// (no terms, no seed files, or every match is already covered).
async fn grep_seed_files(
    checkout_dir: &std::path::Path,
    repo: &str,
    closure: &[ContextEntity],
    patterns: &[String],
    existing_blocks: &[String],
) -> Option<String> {
    // Salient terms: the question's nouns/verbs, regex-escaped for the grep.
    let terms: Vec<String> = salient_terms(patterns)
        .iter()
        .map(|p| regex_lite_escape(p))
        .collect();
    if terms.is_empty() {
        return None;
    }
    let pattern = terms.join("|");

    // Distinct files holding a seed entity, in first-seen order.
    let mut seed_files: Vec<String> = Vec::new();
    let mut seen_files: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for ce in closure {
        if ce.reached_via == ReachedVia::Seed && seen_files.insert(ce.entity.file_path.as_str()) {
            seed_files.push(ce.entity.file_path.clone());
        }
    }
    if seed_files.is_empty() {
        return None;
    }

    // Already-assembled text, to suppress lines the bodies cover.
    let assembled: String = existing_blocks.join("\n");

    let grep = Grep::new(vec![checkout_dir.to_path_buf()]);
    let mut lines: Vec<String> = Vec::new();
    let mut seen_lines: std::collections::HashSet<String> = std::collections::HashSet::new();
    for file in &seed_files {
        if lines.len() >= GREP_FALLBACK_MAX_LINES {
            break;
        }
        let result = grep
            .execute(json!({
                "pattern": pattern,
                "path": file,
                "case_insensitive": true,
                "max_matches": 60,
            }))
            .await
            .ok()?;
        if !result.success {
            continue;
        }
        let Some(matches) = result.output.get("matches").and_then(|m| m.as_array()) else {
            continue;
        };
        for m in matches {
            if lines.len() >= GREP_FALLBACK_MAX_LINES {
                break;
            }
            let (Some(line_no), Some(text)) = (
                m.get("line_number").and_then(|n| n.as_u64()),
                m.get("line").and_then(|t| t.as_str()),
            ) else {
                continue;
            };
            let trimmed = text.trim();
            // Keep only the lines the symbol graph can't represent: doc-comments,
            // bare consts/statics, and module attributes — not ordinary code the
            // assembled bodies already carry.
            if !is_prose_or_decl_line(trimmed) {
                continue;
            }
            if assembled.contains(trimmed) {
                continue; // already covered by an assembled body
            }
            let rendered = format!("{file}:{line_no}: {trimmed}");
            if seen_lines.insert(rendered.clone()) {
                lines.push(rendered);
            }
        }
    }

    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "// {repo}: doc-comments & declarations from the seed files (grep fallback)\n{}",
        lines.join("\n")
    ))
}

/// Whether a trimmed source line is the kind of prose/declaration the symbol
/// graph can't represent and is worth surfacing from a grep: a doc/line comment,
/// a module attribute, or a bare const/static declaration.
fn is_prose_or_decl_line(line: &str) -> bool {
    line.starts_with("///")
        || line.starts_with("//!")
        || line.starts_with("//")
        || line.starts_with("#![")
        || line.starts_with("const ")
        || line.starts_with("pub const ")
        || line.starts_with("static ")
        || line.starts_with("pub static ")
}

/// Escape the regex metacharacters that can appear in an extracted identifier
/// pattern (`.` in path-ish tokens). Identifiers are otherwise regex-safe, so a
/// full regex-escape crate dependency isn't warranted.
fn regex_lite_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if matches!(
                c,
                '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\'
            ) {
                vec!['\\', c]
            } else {
                vec![c]
            }
        })
        .collect()
}

/// One component block: a locator comment then the raw body.
///
/// `// <repo>/<path>:<start>-<end>  <kind> <qualified_name>  [<reached_via>]`
fn format_component(repo: &str, ce: &ContextEntity, body: &str) -> String {
    let e = &ce.entity;
    format!(
        "// {repo}/{path}:{start}-{end}  {kind} {qualified}  [{via}]\n{body}",
        path = e.file_path,
        start = e.start_line,
        end = e.end_line,
        kind = e.kind.as_str(),
        qualified = e.qualified_name,
        via = ce.reached_via.as_str(),
    )
}

// ─── code.context tool ────────────────────────────────────────────────────────

/// `code.context` — assemble the complete relevant source for a code question
/// in one call. Resolves seeds across ALL the agent's indexed repos (from the
/// graph's `repositories` rows, resolver-free) and honours the permission
/// cascade via the access guard.
pub struct CodeContextTool {
    inner: CodeContextService,
    access_guard: Option<KnowledgeAccessGuard>,
}

#[derive(Deserialize)]
struct Input {
    query: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    budget_lines: Option<usize>,
}

impl CodeContextTool {
    pub fn new(service: Arc<KnowledgeService>, agent_workspace: PathBuf) -> Self {
        Self {
            inner: CodeContextService::new(service, agent_workspace),
            access_guard: None,
        }
    }

    /// Scope cross-repo resolution to the repos `agent_id` owns (see
    /// [`CodeContextService::with_agent_scope`]).
    pub fn with_agent_scope(mut self, runner: Arc<MaintenanceRunner>, agent_id: String) -> Self {
        self.inner = self.inner.with_agent_scope(runner, agent_id);
        self
    }

    pub fn with_access_guard(mut self, guard: KnowledgeAccessGuard) -> Self {
        self.access_guard = Some(guard);
        self
    }
}

#[async_trait]
impl Tool for CodeContextTool {
    fn name(&self) -> &str {
        "code.context"
    }

    fn description(&self) -> &str {
        "Assemble the COMPLETE relevant source for a code question in one call — the full body of \
         the most-relevant symbol PLUS its call-graph closure (what it calls, what calls it) PLUS \
         its trait implementations, read from the indexed checkout at HEAD and stamped with the \
         commit. Unlike `knowledge.ask`/`knowledge.symbol` (which return pointers — name + \
         file:line — forcing you to chain follow-up reads), this returns ready-to-read code. The \
         escalation ladder: `repo.grep` to locate a symbol, `repo.read` for one file, and THIS \
         when you need the assembled picture. ALWAYS pass `repo` when the conversation makes the \
         repo clear — a scoped call is much faster and cannot pick the wrong repo. Bounded by \
         `budget_lines` (default 1500). Returns a single labelled block, or a clear message when \
         no relevant code is found."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language code question (e.g. \"how does session resumption work?\")."
                },
                "repo": {
                    "type": "string",
                    "description": "Repo to search, as owner/repo or bare name (the repo.* tools list them). Pass it whenever the conversation makes the repo clear — scoped assembly is much faster and cannot pick the wrong repo. Omit only when the repo is genuinely unknown."
                },
                "budget_lines": {
                    "type": "integer",
                    "description": "Soft cap on total lines of assembled source (default 1500)."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: Input =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;
        if let Some(denied) = maybe_guard(self.access_guard.as_ref(), start) {
            return Ok(denied);
        }
        let budget = input.budget_lines.unwrap_or(DEFAULT_BUDGET_LINES);

        let assembled = match input.repo.as_deref() {
            Some(repo) => match self
                .inner
                .assemble_in_repo(&input.query, repo, budget)
                .await
            {
                Ok(ctx) => ctx,
                Err(message) => {
                    return Ok(ToolResult::failure(
                        message,
                        start.elapsed().as_millis() as u64,
                    ))
                }
            },
            None => self.inner.assemble_for(&input.query, budget).await,
        };
        match assembled {
            Some(ctx) => Ok(ToolResult::success(
                json!({
                    "text": ctx.text,
                    "repo": ctx.repo,
                    "commit": ctx.commit,
                    "components": ctx.components,
                }),
                start.elapsed().as_millis() as u64,
            )),
            None => {
                let searched = input
                    .repo
                    .map(|r| format!(" in repo '{r}'"))
                    .unwrap_or_default();
                Ok(ToolResult::success(
                    json!({
                        "text": format!(
                            "No relevant code found{searched}. The symbol index matched no \
                             entities for this question — try `repo.grep` for prose/config/\
                             comments the symbol graph can't see, or rephrase with a specific \
                             symbol name."
                        ),
                        "components": 0,
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use workforce_codebase_knowledge::graph::entities::{
        CodeEntity, CodeVisibility, EntityKind, Language,
    };
    use workforce_codebase_knowledge::graph::relationships::{Relationship, RelationshipKind};

    #[test]
    fn seeks_code_true_for_code_questions() {
        for q in [
            "how does undoSuspendAccount decide read vs write?",
            "How does the scheduler work in workforce?",
            "what calls process_message",
            "Where is find_entities_by_name defined?",
            "who calls CostCentre?",
            "what module is RepoResolver declared in",
            // Named no symbol and matched no call-graph phrase, so the gate
            // rejected it as "not a code question" and the agent hunted from
            // scratch: 16 rounds, 83 seconds, and the grounding pass then paid
            // the same cost over again.
            "Where do we enforce the tool allowlist, and is there any path that bypasses it?",
            "how is the answer validated before it goes out",
            "which layer authorises a tool call",
            "why is the request routed to the leader",
            "what happens when a session rolls over",
        ] {
            assert!(question_seeks_code(q), "expected a code question: {q}");
        }
    }

    #[test]
    fn seeks_code_false_for_non_code_questions() {
        for q in [
            "look up the NORTH costs in the S3 bucket",
            "did the cost centre sync create the right records?",
            "post the morning briefing",
            "what's the weather today",
            "download main.csv and check the totals",
            "is the v2 report ready",
            // The widened rule must still not fire on ops/data phrasing: an
            // interrogative alone is not intent, and CRUD/ops verbs are shared
            // vocabulary rather than mechanism.
            "which region had the most orders last week",
            "how many invoices were created yesterday",
            "where did the morning briefing go",
            "why is the report late",
        ] {
            assert!(!question_seeks_code(q), "expected a non-code question: {q}");
        }
    }

    #[test]
    fn references_code_symbol_distinguishes_symbols_from_ops_tokens() {
        for t in [
            "undoSuspendAccount",
            "find_entities_by_name",
            "Foo::bar",
            "CostCentre",
        ] {
            assert!(references_code_symbol(t), "expected a symbol: {t}");
        }
        for t in [
            "S3",
            "main.csv",
            "v2",
            "NORTH",
            "bucket",
            "Scheduler",
            "the",
            "report.xlsx",
        ] {
            assert!(!references_code_symbol(t), "expected a non-symbol: {t}");
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git available");
        assert!(out.status.success(), "git {args:?} failed");
    }

    fn init_repo(dir: &Path, files: &[(&str, &str)]) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        git(dir, &["config", "user.name", "t"]);
        for (rel, body) in files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, body).unwrap();
        }
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }

    /// Entity with explicit file + 1-indexed line span.
    #[allow(clippy::too_many_arguments)]
    fn entity(
        id: &str,
        name: &str,
        kind: EntityKind,
        file: &str,
        start: usize,
        end: usize,
    ) -> CodeEntity {
        CodeEntity {
            id: EntityId(id.to_string()),
            repo_id: "r".to_string(),
            kind,
            name: name.to_string(),
            qualified_name: format!("crate::{name}"),
            file_path: file.to_string(),
            start_line: start,
            end_line: end,
            signature: Some(format!("fn {name}()")),
            visibility: CodeVisibility::Public,
            parent_id: None,
            doc_comment: None,
            content_hash: "h".to_string(),
            language: Language::Rust,
        }
    }

    /// Build an in-memory service whose graph carries `entities`/`rels`, with
    /// the entity `repo_id` matching what `repo_id_for(workspace)` derives — so
    /// resolver-less `resolve_query_repo` queries the same repo the entities
    /// live under.
    async fn service_for(
        workspace: &Path,
        entities: &[CodeEntity],
        rels: &[Relationship],
    ) -> Arc<KnowledgeService> {
        let repo_id = KnowledgeService::repo_id_for(workspace);
        let service = Arc::new(KnowledgeService::in_memory(workspace.to_path_buf()).unwrap());
        let graph = service.graph();
        graph
            .register_repo(&repo_id, "Test", &workspace.to_string_lossy())
            .await
            .unwrap();
        // Re-stamp the entities under the workspace repo_id so name/symbol
        // search (which filters by repo_id) finds them.
        let entities: Vec<CodeEntity> = entities
            .iter()
            .cloned()
            .map(|mut e| {
                e.repo_id = repo_id.clone();
                e
            })
            .collect();
        graph.upsert_entities(&entities).await.unwrap();
        graph.upsert_relationships(rels).await.unwrap();
        service
    }

    /// Kotlin entity at a JVM-layout path — the shape that exposed the
    /// Rust-only test filter.
    fn kotlin_entity(
        id: &str,
        name: &str,
        kind: EntityKind,
        file: &str,
        start: usize,
        end: usize,
    ) -> CodeEntity {
        CodeEntity {
            qualified_name: name.to_string(), // the Kotlin walker stores a bare name
            language: Language::Kotlin,
            ..entity(id, name, kind, file, start, end)
        }
    }

    /// The 2026-08-06 production failure, reproduced: a `billing-plans` question
    /// pulled 1196 lines of `StaleOrderReconciliationServiceTest.kt`
    /// because `is_test_entity` only recognised Rust layouts, so no Kotlin test
    /// was ever a test. The implementation must seed; the test must not.
    #[tokio::test]
    async fn kotlin_test_file_is_never_a_seed() {
        let dir = tempfile::tempdir().unwrap();
        let impl_path =
            "billing-plans/src/main/kotlin/com/harbor/billingplans/service/StaleOrderReconciliationService.kt";
        let test_path =
            "billing-plans/src/test/kotlin/com/harbor/billingplans/service/StaleOrderReconciliationServiceTest.kt";
        init_repo(
            dir.path(),
            &[
                (
                    impl_path,
                    "class StaleOrderReconciliationService {\n    fun reconcile() {}\n}\n",
                ),
                (
                    test_path,
                    "class StaleOrderReconciliationServiceTest {\n    fun service() {}\n}\n",
                ),
            ],
        );
        // The test file is listed FIRST, as the higher-ranked seed — in prod it
        // won on a bare `service` method name at the maximum name-match strength.
        let entities = vec![
            kotlin_entity(
                "test-seed",
                "StaleOrderReconciliationServiceTest",
                EntityKind::Class,
                test_path,
                1,
                3,
            ),
            kotlin_entity(
                "impl-seed",
                "StaleOrderReconciliationService",
                EntityKind::Class,
                impl_path,
                1,
                3,
            ),
        ];
        // The test CALLS the implementation, so the graph can reach it as a
        // supplementary neighbour (`CalledBy`) even once it is barred as a seed —
        // the Kotlin walker does extract call edges. Excluding it from the seed
        // list alone would not be enough, so this fixture carries the edge.
        let rels = calls("test-seed", "impl-seed");
        let service = service_for(dir.path(), &entities, &rels).await;
        let svc = CodeContextService::new(service, dir.path().to_path_buf());

        let assembled = svc
            .assemble_for("How does StaleOrderReconciliationService work?", 1500)
            .await
            .expect("the implementation should still ground the answer");

        assert!(
            !assembled.text.contains("src/test/kotlin"),
            "a Kotlin test file was assembled into the prompt:\n{}",
            assembled.text
        );
        assert!(
            assembled.text.contains("src/main/kotlin"),
            "the implementation was dropped — this must not cost recall:\n{}",
            assembled.text
        );
    }

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

    // Seed resolution returns the symbol the question names, and the assembled
    // block carries the header, the locator comment, and the real body.
    #[tokio::test]
    async fn assembles_header_locator_and_body() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(
            dir.path(),
            &[(
                "src/handler.rs",
                // 1: fn process_message() {
                // 2:     validate();
                // 3: }
                "fn process_message() {\n    validate();\n}\n",
            )],
        );
        let entities = vec![entity(
            "seed",
            "process_message",
            EntityKind::Function,
            "src/handler.rs",
            1,
            3,
        )];
        let service = service_for(dir.path(), &entities, &[]).await;
        let svc = CodeContextService::new(service, dir.path().to_path_buf());

        let assembled = svc
            .assemble_for("How does process_message work?", 1500)
            .await
            .expect("seed should resolve");

        assert_eq!(assembled.components, 1);
        assert!(
            assembled.text.contains("component"),
            "header missing: {}",
            assembled.text
        );
        // Locator comment with repo-relative path, span, kind, qualified name,
        // and reached-via tag.
        assert!(
            assembled.text.contains("src/handler.rs:1-3"),
            "locator missing: {}",
            assembled.text
        );
        assert!(
            assembled.text.contains("[seed]"),
            "reached-via tag missing: {}",
            assembled.text
        );
        // The real body, read from the checkout.
        assert!(
            assembled.text.contains("fn process_message()")
                && assembled.text.contains("validate()"),
            "body missing: {}",
            assembled.text
        );
        assert!(!assembled.commit.is_empty(), "commit should be stamped");
        // A deterministic citation for the seed entity rides alongside the text:
        // the file/span/kind/symbol come straight from the entity. The repo's
        // source isn't a resolvable GitHub target in tests, so `owner` is empty
        // and the UI falls back to non-link text.
        assert_eq!(assembled.citations.len(), 1, "one seed citation");
        let cite = &assembled.citations[0];
        assert_eq!(cite.path, "src/handler.rs");
        assert_eq!((cite.start_line, cite.end_line), (1, 3));
        assert_eq!(cite.kind, "function");
        assert_eq!(cite.symbol, "crate::process_message");
        assert!(cite.owner.is_empty(), "no GitHub owner in test scope");
        assert_eq!(cite.commit, assembled.commit, "citation pins the commit");
    }

    // The closure pulls in a CROSS-FILE callee's body alongside the seed (the
    // callee lives in a different file, so it's reached via the call graph, not
    // the same-file co-pull).
    #[tokio::test]
    async fn assembles_seed_plus_callee_bodies() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(
            dir.path(),
            &[
                ("src/lib.rs", "fn outer() {\n    inner();\n}\n"),
                ("src/inner.rs", "fn inner() {\n    let x = 1;\n}\n"),
            ],
        );
        let entities = vec![
            entity("seed", "outer", EntityKind::Function, "src/lib.rs", 1, 3),
            entity(
                "callee",
                "inner",
                EntityKind::Function,
                "src/inner.rs",
                1,
                3,
            ),
        ];
        let rels: Vec<Relationship> = calls("seed", "callee").into();
        let service = service_for(dir.path(), &entities, &rels).await;
        let svc = CodeContextService::new(service, dir.path().to_path_buf());

        let assembled = svc.assemble_for("what does outer do?", 1500).await.unwrap();
        assert_eq!(assembled.components, 2, "seed + callee");
        assert!(assembled.text.contains("fn outer()"));
        assert!(assembled.text.contains("fn inner()"));
        assert!(assembled.text.contains("[seed]"));
        assert!(assembled.text.contains("[callee]"));
        // The callee's body IS assembled, but a callee is supplementary, so it is
        // NOT cited — only the seed is. (Callees/callers are noisy as citations.)
        let cited: Vec<&str> = assembled
            .citations
            .iter()
            .map(|c| c.symbol.as_str())
            .collect();
        assert_eq!(
            cited,
            vec!["crate::outer"],
            "only the seed is cited, not the callee"
        );
    }

    // The closure co-pulls a SAME-FILE sibling definition (a const/fn the reader
    // needs) even with no call edge between them — the completeness fix.
    #[tokio::test]
    async fn assembles_same_file_sibling() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(
            dir.path(),
            &[(
                "src/vault.rs",
                "const SALT_LEN: usize = 16;\n\nfn derive_key() {\n    let n = SALT_LEN;\n}\n\nfn encrypt_blob() {\n    derive_key();\n}\n",
            )],
        );
        // Seed = encrypt_blob (lines 6-8); sibling derive_key (3-5) and SALT_LEN
        // (1) share the file but have NO call edge to the seed.
        let entities = vec![
            entity(
                "seed",
                "encrypt_blob",
                EntityKind::Function,
                "src/vault.rs",
                6,
                8,
            ),
            entity(
                "derive",
                "derive_key",
                EntityKind::Function,
                "src/vault.rs",
                3,
                5,
            ),
            entity(
                "salt",
                "SALT_LEN",
                EntityKind::Constant,
                "src/vault.rs",
                1,
                1,
            ),
        ];
        let service = service_for(dir.path(), &entities, &[]).await;
        let svc = CodeContextService::new(service, dir.path().to_path_buf());

        let assembled = svc
            .assemble_for("how does encrypt_blob work?", 1500)
            .await
            .unwrap();
        assert!(
            assembled.text.contains("fn derive_key()"),
            "same-file fn must be co-pulled: {}",
            assembled.text
        );
        assert!(
            assembled.text.contains("const SALT_LEN"),
            "same-file const must be co-pulled: {}",
            assembled.text
        );
        assert!(
            assembled.text.contains("[file]"),
            "file-context tag: {}",
            assembled.text
        );
        // The seed AND its same-file context (FileContext) are both cited, and
        // the seed leads — citations are ordered Seed-first.
        let cited: Vec<&str> = assembled
            .citations
            .iter()
            .map(|c| c.symbol.as_str())
            .collect();
        assert_eq!(
            cited[0], "crate::encrypt_blob",
            "the seed must lead the citations: {cited:?}"
        );
        assert!(
            cited.contains(&"crate::derive_key") && cited.contains(&"crate::SALT_LEN"),
            "same-file context must be cited: {cited:?}"
        );
    }

    // Grep fallback: a module-level `//!` doc-comment — which the symbol graph
    // can't represent (it's not on any indexed entity's body) — is surfaced from
    // the seed file when it contains a question term.
    #[tokio::test]
    async fn grep_fallback_surfaces_module_doc_comment() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(
            dir.path(),
            &[(
                "src/vault.rs",
                // Module doc-comment on line 1 mentions "encrypt"; the seed body
                // (lines 3-5) does not, so only the grep fallback can surface it.
                "//! On-disk format: magic || salt || nonce || ciphertext, encrypt with AES-GCM.\n\nfn encrypt_blob() {\n    let x = 1;\n}\n",
            )],
        );
        let entities = vec![entity(
            "seed",
            "encrypt_blob",
            EntityKind::Function,
            "src/vault.rs",
            3,
            5,
        )];
        let service = service_for(dir.path(), &entities, &[]).await;
        let svc = CodeContextService::new(service, dir.path().to_path_buf());

        let assembled = svc
            .assemble_for("how does encrypt work?", 1500)
            .await
            .unwrap();
        assert!(
            assembled.text.contains("grep fallback"),
            "grep-fallback block missing: {}",
            assembled.text
        );
        assert!(
            assembled.text.contains("On-disk format") && assembled.text.contains("AES-GCM"),
            "module doc-comment not surfaced by grep fallback: {}",
            assembled.text
        );
    }

    // Two entities whose read bodies are byte-identical collapse to one
    // component; distinct bodies are both kept.
    #[tokio::test]
    async fn dedups_identical_bodies() {
        let dir = tempfile::tempdir().unwrap();
        // alpha (lines 1-2) and beta (lines 5-6) have byte-identical bodies
        // (line 4 is the blank separator).
        init_repo(
            dir.path(),
            &[(
                "src/dup.rs",
                "fn shared_logic() {\n    noop();\n}\n\nfn shared_logic() {\n    noop();\n}\n",
            )],
        );
        // alpha (seed, 1-2) calls beta (callee, 5-6); identical 2-line bodies.
        let entities = vec![
            entity("seed", "alpha", EntityKind::Function, "src/dup.rs", 1, 2),
            entity("callee", "beta", EntityKind::Function, "src/dup.rs", 5, 6),
        ];
        let rels: Vec<Relationship> = calls("seed", "callee").into();
        let service = service_for(dir.path(), &entities, &rels).await;
        let svc = CodeContextService::new(service, dir.path().to_path_buf());

        let assembled = svc.assemble_for("alpha", 1500).await.unwrap();
        assert_eq!(
            assembled.components, 1,
            "identical bodies deduped to one: {}",
            assembled.text
        );
    }

    // RELEVANCE SELF-GATE: a clearly non-code question whose only token brushes a
    // symbol via a buried substring (name-match 2.0, below the 3.0 word-boundary
    // floor the grounding decision counts) returns None — so the live answer path
    // injects NOTHING for it. The seed DOES resolve (substring search hit),
    // proving the grounding gate — not the empty-seeds path — is what rejects it.
    #[tokio::test]
    async fn weak_substring_match_is_gated_to_none() {
        let dir = tempfile::tempdir().unwrap();
        // "policy" sits BURIED inside "policyset": preceded by `_` but followed by
        // `s`, so it is neither a prefix/suffix nor a word-boundary match — a true
        // buried substring scoring 2.0.
        init_repo(
            dir.path(),
            &[(
                "src/lib.rs",
                "fn enforce_policyset() {\n    let x = 1;\n}\n",
            )],
        );
        let entities = vec![entity(
            "seed",
            "enforce_policyset",
            EntityKind::Function,
            "src/lib.rs",
            1,
            3,
        )];
        let service = service_for(dir.path(), &entities, &[]).await;
        let svc = CodeContextService::new(service, dir.path().to_path_buf());

        let assembled = svc
            .assemble_for("what is the escalation policy for on-call?", 1500)
            .await;
        assert!(
            assembled.is_none(),
            "a weak buried-substring brush must be gated out: {assembled:?}"
        );
    }

    // The complement of the gate: a genuine code question that NAMES the symbol
    // (exact match, name-match 5.0) clears the grounding gate and returns Some —
    // the injection fires for real code questions.
    #[tokio::test]
    async fn exact_name_match_clears_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(
            dir.path(),
            &[(
                "src/lib.rs",
                "fn enforce_policyset() {\n    let x = 1;\n}\n",
            )],
        );
        let entities = vec![entity(
            "seed",
            "enforce_policyset",
            EntityKind::Function,
            "src/lib.rs",
            1,
            3,
        )];
        let service = service_for(dir.path(), &entities, &[]).await;
        let svc = CodeContextService::new(service, dir.path().to_path_buf());

        // Names the symbol exactly → name-match 5.0, well over the gate.
        let assembled = svc
            .assemble_for("how does enforce_policyset work?", 1500)
            .await;
        assert!(
            assembled.is_some(),
            "an exact-name code question must clear the gate"
        );
    }

    // No seed resolves → None (caller falls back to grep).
    #[tokio::test]
    async fn no_seed_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), &[("src/lib.rs", "fn unrelated() {}\n")]);
        let entities = vec![entity(
            "seed",
            "unrelated",
            EntityKind::Function,
            "src/lib.rs",
            1,
            1,
        )];
        let service = service_for(dir.path(), &entities, &[]).await;
        let svc = CodeContextService::new(service, dir.path().to_path_buf());

        // A question whose tokens match no entity name pattern.
        let assembled = svc.assemble_for("zzz_nonexistent_symbol_qqq", 1500).await;
        assert!(assembled.is_none(), "no matching seed → None");
    }

    // The tool reports a clear "no relevant code found" message on a miss.
    #[tokio::test]
    async fn tool_reports_no_code_found() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), &[("src/lib.rs", "fn unrelated() {}\n")]);
        let entities = vec![entity(
            "seed",
            "unrelated",
            EntityKind::Function,
            "src/lib.rs",
            1,
            1,
        )];
        let service = service_for(dir.path(), &entities, &[]).await;
        let tool = CodeContextTool::new(service, dir.path().to_path_buf());

        let result = tool
            .execute(json!({ "query": "zzz_nonexistent_symbol_qqq" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(
            result.output.get("components").and_then(|c| c.as_u64()),
            Some(0)
        );
        let text = result.output.get("text").and_then(|t| t.as_str()).unwrap();
        assert!(text.contains("No relevant code found"), "text: {text}");
    }

    // The injected block carries the authoritative header, the assembled source,
    // then the answer-first reminder — so when the caller appends the user's
    // question, the obligation sits adjacent to it rather than a thousand lines
    // above.
    #[test]
    fn injected_block_is_header_then_code_then_answer_first_reminder() {
        let ctx = "// 1 component from workforce @ abc123\nfn f() {}";
        let out = build_injected_block(ctx);
        assert!(out.starts_with(INJECTION_HEADER), "header must lead: {out}");
        let header_pos = out.find(INJECTION_HEADER).unwrap();
        let code_pos = out.find("fn f() {}").unwrap();
        let reminder_pos = out.find(INJECTION_ANSWER_FIRST).unwrap();
        assert!(
            header_pos < code_pos && code_pos < reminder_pos,
            "order must be header → code → reminder: {out}"
        );
        assert!(
            out.ends_with(INJECTION_ANSWER_FIRST),
            "the reminder must be the LAST thing before the question the caller appends"
        );
    }

    // The header binds the model to the block: answer from it, tools only for
    // gaps, and never assert absence from a selected subset. The previous header
    // invited tool use in both branches, and a production trace showed a model
    // re-investigating for 17 rounds with the answer already injected.
    #[test]
    fn header_is_answer_first_and_scopes_tools_to_gaps() {
        assert!(
            INJECTION_HEADER.contains("Answer directly FROM this source"),
            "answer-first obligation must be explicit"
        );
        assert!(
            INJECTION_HEADER.contains("ONLY for what this block does not contain"),
            "tool use must be scoped to gaps"
        );
        assert!(
            INJECTION_HEADER.contains("Do not re-read or re-search"),
            "re-derivation of injected content must be forbidden"
        );
        assert!(
            INJECTION_HEADER.contains("cannot prove absence"),
            "universal-negative claims must require a search beyond the block"
        );
    }

    // An absurdly large assembled block is truncated (never errors) so it can't
    // blow the model's context window.
    #[test]
    fn injected_content_caps_a_runaway_block() {
        let huge = (0..(INJECTION_MAX_LINES + 500))
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = build_injected_block(&huge);
        assert!(out.contains("assembled context truncated"), "must mark cut");
        // The kept lines + header + truncation marker + reminder stay bounded.
        assert!(
            out.lines().count() <= INJECTION_MAX_LINES + 5,
            "capped output must stay bounded: {} lines",
            out.lines().count()
        );
        assert!(
            out.ends_with(INJECTION_ANSWER_FIRST),
            "reminder still trails the cut"
        );
    }

    /// Stamp an entity's repo_id explicitly — for the multi-repo test where two
    /// repos coexist in one graph and `repo_id` decides which checkout a body is
    /// read from.
    fn entity_in_repo(
        id: &str,
        name: &str,
        kind: EntityKind,
        file: &str,
        start: usize,
        end: usize,
        repo_id: &str,
    ) -> CodeEntity {
        let mut e = entity(id, name, kind, file, start, end);
        e.repo_id = repo_id.to_string();
        e
    }

    // MULTI-REPO: two repos are indexed in one store; the question's symbol lives
    // only in repo B. `assemble_for` must resolve the seed from repo B, read its
    // body from repo B's OWN checkout, and report repo B as the source — without
    // any `repo` argument or resolver. Repo A (unrelated billing code) is searched
    // but contributes nothing.
    #[tokio::test]
    async fn assembles_across_multiple_repos() {
        // Repo A: billing checkout.
        let repo_a = tempfile::tempdir().unwrap();
        init_repo(
            repo_a.path(),
            &[(
                "src/billing.rs",
                "fn render_invoice() {\n    let x = 1;\n}\n",
            )],
        );
        // Repo B: scheduler checkout — holds the symbol the question is about.
        let repo_b = tempfile::tempdir().unwrap();
        init_repo(
            repo_b.path(),
            &[(
                "src/scheduler.rs",
                "fn scheduler_tick() {\n    persist_jobs();\n}\n",
            )],
        );

        let repo_a_id = KnowledgeService::repo_id_for(repo_a.path());
        let repo_b_id = KnowledgeService::repo_id_for(repo_b.path());

        // The KnowledgeService workspace is irrelevant to resolution here (it's
        // driven by the registered `repositories` rows); use repo A's path.
        let service = Arc::new(KnowledgeService::in_memory(repo_a.path().to_path_buf()).unwrap());
        let graph = service.graph();
        graph
            .register_repo(&repo_a_id, "orbit", &repo_a.path().to_string_lossy())
            .await
            .unwrap();
        graph
            .register_repo(&repo_b_id, "workforce", &repo_b.path().to_string_lossy())
            .await
            .unwrap();
        let entities = vec![
            entity_in_repo(
                "a1",
                "render_invoice",
                EntityKind::Function,
                "src/billing.rs",
                1,
                3,
                &repo_a_id,
            ),
            entity_in_repo(
                "b1",
                "scheduler_tick",
                EntityKind::Function,
                "src/scheduler.rs",
                1,
                3,
                &repo_b_id,
            ),
        ];
        graph.upsert_entities(&entities).await.unwrap();

        // Both repos are in this agent's scope.
        let svc = CodeContextService::new(Arc::clone(&service), repo_a.path().to_path_buf())
            .with_repo_scope([repo_a_id.clone(), repo_b_id.clone()]);
        let assembled = svc
            .assemble_for("how does scheduler_tick work?", 1500)
            .await
            .expect("seed resolves from repo B across both indexed repos");

        // The scheduler body was read from repo B's checkout, and repo B is the
        // reported source — even though repo A was searched too.
        assert!(
            assembled.text.contains("fn scheduler_tick()"),
            "scheduler body from repo B missing: {}",
            assembled.text
        );
        assert_eq!(
            assembled.repo, "workforce",
            "primary repo should be the one holding the winning seed"
        );
        // Repo A's unrelated billing code must not be pulled in.
        assert!(
            !assembled.text.contains("fn render_invoice()"),
            "unrelated repo-A code must not appear: {}",
            assembled.text
        );
    }

    fn selector_repo(name: &str, owner: &str, repo_name: &str) -> IndexedRepo {
        IndexedRepo {
            repo_id: format!("id-{name}"),
            name: name.to_string(),
            checkout_dir: PathBuf::from("/tmp"),
            commit: String::new(),
            owner: owner.to_string(),
            repo_name: repo_name.to_string(),
        }
    }

    #[test]
    fn repo_selector_resolves_exact_bare_ambiguous_and_unknown() {
        let repos = vec![
            selector_repo("harborlabs/core-api", "harborlabs", "core-api"),
            selector_repo("harborlabs/api", "harborlabs", "api"),
            selector_repo("driftwerk/api", "driftwerk", "api"),
            selector_repo("workforce", "", ""),
        ];

        // Exact owner/repo display, owner case-insensitive.
        assert_eq!(
            resolve_repo_selector(&repos, "HarborLabs/core-api")
                .unwrap()
                .repo_id,
            "id-harborlabs/core-api"
        );
        // Stored name when no GitHub coordinates exist.
        assert_eq!(
            resolve_repo_selector(&repos, "workforce").unwrap().repo_id,
            "id-workforce"
        );
        // Bare repo name, unique across owners.
        assert_eq!(
            resolve_repo_selector(&repos, "core-api").unwrap().repo_id,
            "id-harborlabs/core-api"
        );

        let err = resolve_repo_selector(&repos, "api").unwrap_err();
        assert!(err.contains("'api' is ambiguous"), "{err}");
        assert!(err.contains("driftwerk/api") && err.contains("harborlabs/api"));

        let err = resolve_repo_selector(&repos, "nope").unwrap_err();
        assert!(err.contains("unknown repo 'nope'"), "{err}");
        assert!(err.contains("workforce") && err.contains("harborlabs/core-api"));
    }

    #[test]
    fn an_agent_with_no_attached_repo_is_pointed_at_the_repo_tools() {
        let err = resolve_repo_selector(&[], "anything").unwrap_err();
        assert!(err.contains("no indexed repo is attached"), "{err}");
        assert!(err.contains("Do not retry"), "{err}");
        assert!(
            ["repo.list", "repo.grep", "repo.read"]
                .iter()
                .all(|tool| err.contains(tool)),
            "{err}"
        );
    }

    #[tokio::test]
    async fn explicit_repo_beats_cross_repo_ranking() {
        // The SAME symbol exists in both repos; the pinned repo's body must be
        // the one assembled, regardless of which repo ranking would pick.
        let repo_a = tempfile::tempdir().unwrap();
        init_repo(
            repo_a.path(),
            &[(
                "src/billing.rs",
                "fn render_invoice() {\n    let from_a = 1;\n}\n",
            )],
        );
        let repo_b = tempfile::tempdir().unwrap();
        init_repo(
            repo_b.path(),
            &[(
                "src/billing.rs",
                "fn render_invoice() {\n    let from_b = 2;\n}\n",
            )],
        );
        let repo_a_id = KnowledgeService::repo_id_for(repo_a.path());
        let repo_b_id = KnowledgeService::repo_id_for(repo_b.path());
        let service = Arc::new(KnowledgeService::in_memory(repo_a.path().to_path_buf()).unwrap());
        let graph = service.graph();
        graph
            .register_repo(&repo_a_id, "orbit", &repo_a.path().to_string_lossy())
            .await
            .unwrap();
        graph
            .register_repo(&repo_b_id, "workforce", &repo_b.path().to_string_lossy())
            .await
            .unwrap();
        graph
            .upsert_entities(&[
                entity_in_repo(
                    "a1",
                    "render_invoice",
                    EntityKind::Function,
                    "src/billing.rs",
                    1,
                    3,
                    &repo_a_id,
                ),
                entity_in_repo(
                    "b1",
                    "render_invoice",
                    EntityKind::Function,
                    "src/billing.rs",
                    1,
                    3,
                    &repo_b_id,
                ),
            ])
            .await
            .unwrap();

        let svc = CodeContextService::new(Arc::clone(&service), repo_a.path().to_path_buf())
            .with_repo_scope([repo_a_id.clone(), repo_b_id.clone()]);

        let assembled = svc
            .assemble_in_repo("how does render_invoice work?", "orbit", 1500)
            .await
            .unwrap()
            .expect("pinned repo assembles its own symbol");
        assert_eq!(assembled.repo, "orbit");
        assert!(assembled.text.contains("from_a"), "{}", assembled.text);
        assert!(!assembled.text.contains("from_b"), "{}", assembled.text);

        // The pinned repo holds nothing for this question → clean None, and an
        // unknown selector is a teaching error, not a silent fallback.
        assert!(svc
            .assemble_in_repo("how does scheduler_tick work?", "orbit", 1500)
            .await
            .unwrap()
            .is_none());
        let err = svc
            .assemble_in_repo("how does render_invoice work?", "nope", 1500)
            .await
            .unwrap_err();
        assert!(err.contains("unknown repo 'nope'"), "{err}");
        assert!(err.contains("orbit") && err.contains("workforce"));
    }

    #[tokio::test]
    async fn tool_schema_exposes_repo_scoping() {
        let workspace = tempfile::tempdir().unwrap();
        let service =
            Arc::new(KnowledgeService::in_memory(workspace.path().to_path_buf()).unwrap());
        let tool = CodeContextTool::new(service, workspace.path().to_path_buf());
        let schema = tool.input_schema();
        assert!(schema["properties"]["repo"]["description"]
            .as_str()
            .unwrap()
            .contains("cannot pick the wrong repo"));
        assert!(tool.description().contains("pass `repo`"));
    }

    // SCOPE ENFORCEMENT: a repo present in the shared store but NOT in this
    // agent's scope is never searched — even if it holds the only match for the
    // question. This is the cross-agent-leak guard: the shared codegraph must not
    // assemble another agent's code.
    #[tokio::test]
    async fn out_of_scope_repo_is_not_searched() {
        // Repo A: this agent's workspace (no scheduler code).
        let repo_a = tempfile::tempdir().unwrap();
        init_repo(
            repo_a.path(),
            &[(
                "src/billing.rs",
                "fn render_invoice() {\n    let x = 1;\n}\n",
            )],
        );
        // Repo B: ANOTHER agent's repo, holding the scheduler the question asks
        // about — but it is NOT in this agent's scope.
        let repo_b = tempfile::tempdir().unwrap();
        init_repo(
            repo_b.path(),
            &[(
                "src/scheduler.rs",
                "fn scheduler_tick() {\n    let y = 2;\n}\n",
            )],
        );
        let repo_a_id = KnowledgeService::repo_id_for(repo_a.path());
        let repo_b_id = KnowledgeService::repo_id_for(repo_b.path());

        let service = Arc::new(KnowledgeService::in_memory(repo_a.path().to_path_buf()).unwrap());
        let graph = service.graph();
        graph
            .register_repo(&repo_a_id, "mine", &repo_a.path().to_string_lossy())
            .await
            .unwrap();
        graph
            .register_repo(
                &repo_b_id,
                "other-agents-repo",
                &repo_b.path().to_string_lossy(),
            )
            .await
            .unwrap();
        graph
            .upsert_entities(&[
                entity_in_repo(
                    "a1",
                    "render_invoice",
                    EntityKind::Function,
                    "src/billing.rs",
                    1,
                    3,
                    &repo_a_id,
                ),
                entity_in_repo(
                    "b1",
                    "scheduler_tick",
                    EntityKind::Function,
                    "src/scheduler.rs",
                    1,
                    3,
                    &repo_b_id,
                ),
            ])
            .await
            .unwrap();

        // Scope = repo A only (workspace). Repo B is out of scope.
        let svc = CodeContextService::new(Arc::clone(&service), repo_a.path().to_path_buf())
            .with_repo_scope([repo_a_id.clone()]);
        let assembled = svc
            .assemble_for("how does scheduler_tick work?", 1500)
            .await;

        // The scheduler lives only in the out-of-scope repo B, so nothing relevant
        // resolves — repo B's code must never appear.
        match assembled {
            None => {} // acceptable: no in-scope match
            Some(ctx) => assert!(
                !ctx.text.contains("fn scheduler_tick()"),
                "out-of-scope repo B's code leaked into the result: {}",
                ctx.text
            ),
        }
    }

    // CITATION FILTER: only the directly-relevant `reached_via` kinds are cited;
    // the supplementary call-graph kinds are assembled but never cited.
    #[test]
    fn only_directly_relevant_entities_are_cited() {
        assert_eq!(citation_rank(ReachedVia::Seed), Some(0));
        assert_eq!(citation_rank(ReachedVia::FileContext), Some(1));
        assert_eq!(citation_rank(ReachedVia::Type), Some(2));
        // Supplementary — assembled but not cited.
        assert_eq!(citation_rank(ReachedVia::Callee), None);
        assert_eq!(citation_rank(ReachedVia::Caller), None);
        assert_eq!(citation_rank(ReachedVia::Test), None);
        assert_eq!(citation_rank(ReachedVia::Impl), None);
    }

    // CITATION SHAPE: `citation_for` carries the entity's file/span/kind/symbol
    // and the repo's GitHub owner/repo/commit — the permalink coordinates the UI
    // turns into a `github.com/{owner}/{repo}/blob/{commit}/{path}#L..` link.
    #[test]
    fn citation_for_threads_github_coordinates() {
        let repo = IndexedRepo {
            repo_id: "rid".to_string(),
            name: "workforce".to_string(),
            checkout_dir: PathBuf::from("/tmp/checkout"),
            commit: "abc123".to_string(),
            owner: "driftwerk".to_string(),
            repo_name: "workforce".to_string(),
        };
        let ce = ContextEntity {
            entity: entity(
                "seed",
                "encrypt_blob",
                EntityKind::Function,
                "crates/hub/src/vault.rs",
                377,
                401,
            ),
            reached_via: ReachedVia::Seed,
        };
        let cite = citation_for(&repo, &ce);
        assert_eq!(cite.owner, "driftwerk");
        assert_eq!(cite.repo_name, "workforce");
        assert_eq!(cite.commit, "abc123");
        assert_eq!(cite.path, "crates/hub/src/vault.rs");
        assert_eq!((cite.start_line, cite.end_line), (377, 401));
        assert_eq!(cite.kind, "function");
        assert_eq!(cite.symbol, "crate::encrypt_blob");
        assert_eq!(cite.repo, "workforce");
    }

    // CITATION CAP + DEDUPE: many seeds in one file dedupe by (path, start_line)
    // and the list is capped at MAX_CITATIONS, Seed-first.
    #[tokio::test]
    async fn citations_dedupe_by_location_and_cap() {
        let dir = tempfile::tempdir().unwrap();
        // 20 distinct one-line functions → 20 entities; each body distinct so the
        // body-dedup keeps them all, exercising the citation cap (12).
        let mut file = String::new();
        for i in 0..20 {
            file.push_str(&format!("fn handler_{i}() {{ let x = {i}; }}\n"));
        }
        init_repo(dir.path(), &[("src/handlers.rs", file.as_str())]);
        let entities: Vec<CodeEntity> = (0..20)
            .map(|i| {
                entity(
                    &format!("e{i}"),
                    &format!("handler_{i}"),
                    EntityKind::Function,
                    "src/handlers.rs",
                    i + 1,
                    i + 1,
                )
            })
            .collect();
        let service = service_for(dir.path(), &entities, &[]).await;
        let svc = CodeContextService::new(service, dir.path().to_path_buf());

        let assembled = svc
            .assemble_for("how do the handler functions work?", 1500)
            .await;
        if let Some(ctx) = assembled {
            assert!(
                ctx.citations.len() <= MAX_CITATIONS,
                "citation list must be capped at {MAX_CITATIONS}: got {}",
                ctx.citations.len()
            );
            // No two citations share a (path, start_line).
            let mut seen = std::collections::HashSet::new();
            for c in &ctx.citations {
                assert!(
                    seen.insert((c.path.clone(), c.start_line)),
                    "duplicate citation location {}:{}",
                    c.path,
                    c.start_line
                );
            }
        }
    }

    // ---- Grounding decision (pure `rank_repos` + `decide`) ----
    //
    // These drive the inject-or-not / which-repo policy in isolation, encoding the
    // real prod firings the fix targets (session ccde1f0e): a conversational "…get
    // write capability" must NOT inject the hub, while "how does RateLimitGate work"
    // must inject the repo that holds it.
    use workforce_codebase_knowledge::query::{PatternHit, ScoredSeed};

    fn seed(
        repo: &str,
        name: &str,
        name_match: f32,
        vector: f32,
        hits: &[(usize, f32)],
    ) -> ScoredSeed {
        ScoredSeed {
            entity: entity_in_repo("e", name, EntityKind::Function, "src/x.rs", 1, 3, repo),
            name_match,
            vector,
            relevance: name_match + vector,
            pattern_hits: hits
                .iter()
                .map(|&(pattern_idx, strength)| PatternHit {
                    pattern_idx,
                    strength,
                })
                .collect(),
        }
    }

    fn resolution(
        seeds: Vec<ScoredSeed>,
        repo_count: usize,
        pattern_df: Vec<u16>,
    ) -> SeedResolution {
        SeedResolution {
            seeds,
            repo_count,
            pattern_df,
            vector_configured: false,
        }
    }

    fn ground_with_question(
        res: &SeedResolution,
        patterns: &[&str],
        question: &str,
    ) -> GroundingDecision {
        let cfg = GroundingConfig::default();
        let patterns: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        let idents = identifier_tokens(question);
        let ranked = rank_repos(res, &patterns, &idents, &cfg);
        decide(&ranked, res.repo_count, &cfg)
    }

    fn ground(res: &SeedResolution, patterns: &[&str]) -> GroundingDecision {
        ground_with_question(res, patterns, &patterns.join(" "))
    }

    fn injected_repos(d: &GroundingDecision) -> Vec<String> {
        match d {
            GroundingDecision::Inject { repos } => repos.clone(),
            GroundingDecision::Skip { reason } => panic!("expected Inject, got Skip({reason})"),
        }
    }

    // THE PROD BUG: "…get write capability" — "capability" is a bare English word
    // that exact-matches the hub-only `Capability` enum (df 1 → high IDF), but its
    // bare shape keeps its contribution small, so the hub is NOT injected.
    #[test]
    fn generic_word_exact_collision_skips() {
        let res = resolution(
            vec![seed("hub", "Capability", 5.0, 0.0, &[(0, 5.0)])],
            2,
            vec![1, 2],
        );
        assert!(
            matches!(
                ground(&res, &["capability", "write"]),
                GroundingDecision::Skip { .. }
            ),
            "a bare-word collision on a hub-only symbol must not inject the hub"
        );
    }

    // RECALL GUARD: "how does RateLimitGate work" — an identifier-shaped exact match
    // in a single repo injects that repo even when IDF is modest (few repos).
    #[test]
    fn rare_identifier_exact_injects() {
        let res = resolution(
            vec![seed("core-api", "RateLimitGate", 5.0, 0.0, &[(0, 5.0)])],
            2,
            vec![1],
        );
        assert_eq!(
            injected_repos(&ground(&res, &["RateLimitGate"])),
            vec!["core-api".to_string()]
        );
    }

    // RECALL GUARD (camelCase): the planner lowercases a leading-lowercase
    // `camelCase` reference, so shape must be recovered from the original question
    // — otherwise a named symbol like `undoSuspendAccount` wrongly Skips.
    #[test]
    fn camelcase_symbol_injects() {
        let res = resolution(
            vec![seed(
                "core-api",
                "undoSuspendAccount",
                5.0,
                0.0,
                &[(0, 5.0)],
            )],
            3,
            vec![1],
        );
        assert_eq!(
            injected_repos(&ground_with_question(
                &res,
                &["undosuspendaccount"],
                "how does undoSuspendAccount work",
            )),
            vec!["core-api".to_string()]
        );
    }

    // PRECISION GUARD: a generic identifier-shaped symbol present in EVERY repo
    // (`read_file`, df == repo_count → IDF 0) must not admit via `best_specific`
    // and inject an arbitrary (alphabetically-first) repo.
    #[test]
    fn generic_snake_case_no_arbitrary_repo() {
        let res = resolution(
            vec![
                seed("aaa", "read_file", 5.0, 0.0, &[(0, 5.0)]),
                seed("bbb", "read_file", 5.0, 0.0, &[(0, 5.0)]),
                seed("ccc", "read_file", 5.0, 0.0, &[(0, 5.0)]),
            ],
            3,
            vec![3],
        );
        assert!(
            matches!(ground(&res, &["read_file"]), GroundingDecision::Skip { .. }),
            "a generic identifier in every repo must not inject an arbitrary repo"
        );
    }

    // Two rare identifier tokens agreeing in ONE repo inject it (corroboration);
    // the same strength spread one-token-per-repo clears nothing.
    #[test]
    fn corroborating_tokens_inject_scattered_skip() {
        let agree = resolution(
            vec![seed("core-api", "X", 5.0, 0.0, &[(0, 5.0), (1, 5.0)])],
            3,
            vec![1, 1],
        );
        assert_eq!(
            injected_repos(&ground(&agree, &["RateLimitGate", "undoSuspendAccount"])),
            vec!["core-api".to_string()]
        );

        let scattered = resolution(
            vec![
                seed("a", "s", 3.5, 0.0, &[(0, 3.5)]),
                seed("b", "r", 3.5, 0.0, &[(1, 3.5)]),
                seed("c", "c", 3.5, 0.0, &[(2, 3.5)]),
            ],
            3,
            vec![1, 1, 1],
        );
        assert!(matches!(
            ground(&scattered, &["order", "catalogue", "compliance"]),
            GroundingDecision::Skip { .. }
        ));
    }

    // REPO CONCENTRATION: a noise secondary (weak, wrong repo) is dropped by the
    // margin; a genuinely-competitive secondary is kept.
    #[test]
    fn concentration_drops_noise_keeps_competitive() {
        let noisy = resolution(
            vec![
                seed("core-api", "RateLimitGate", 5.0, 0.0, &[(0, 5.0)]),
                seed("hub", "read_file", 3.0, 0.0, &[(1, 3.0)]),
            ],
            2,
            vec![1, 1],
        );
        assert_eq!(
            injected_repos(&ground(&noisy, &["RateLimitGate", "read"])),
            vec!["core-api".to_string()]
        );

        let competitive = resolution(
            vec![
                seed("core-api", "FooType", 5.0, 0.0, &[(0, 5.0)]),
                seed("edge", "BarType", 5.0, 0.0, &[(1, 5.0)]),
            ],
            2,
            vec![1, 1],
        );
        assert_eq!(
            injected_repos(&ground(&competitive, &["FooType", "BarType"])).len(),
            2
        );
    }

    // SINGLE-REPO legacy path: with one repo, IDF is uninformative, so a plain
    // name-match at word-boundary strength still injects (recall preserved).
    #[test]
    fn single_repo_admits_on_name_match() {
        let res = resolution(
            vec![seed("only", "scheduler", 3.5, 0.0, &[(0, 3.5)])],
            1,
            vec![1],
        );
        assert_eq!(
            injected_repos(&ground(&res, &["scheduler"])),
            vec!["only".to_string()]
        );
    }

    // A single multi-word symbol matching "read"+"gate" from ONE seed does not
    // free-pass: "read" is corpus-common (df high → IDF ~0) and the bare parts
    // stay below the gate unless the whole identifier is named.
    #[test]
    fn multiword_self_cover_no_free_pass() {
        let res = resolution(
            vec![seed(
                "hub",
                "RateLimitGate",
                3.5,
                0.0,
                &[(0, 3.5), (1, 3.5)],
            )],
            2,
            vec![2, 1],
        );
        assert!(matches!(
            ground(&res, &["read", "gate"]),
            GroundingDecision::Skip { .. }
        ));
    }

    // Word-boundary matching: a word that merely EMBEDS a trigger must not
    // fire the gate — each false pass costs a thousand-line injection of
    // irrelevant code into the turn.
    #[test]
    fn embedded_trigger_words_do_not_fire_the_gate() {
        for q in [
            // "how does" inside "show doesn't"
            "the show doesn't start until nine, right?",
            // "what" inside "whatsapp" (and "trigger" needs an interrogative)
            "send the whatsapp reminder about triggering the payroll run",
            // "how" inside "showcase"
            "showcase the new dashboard to the trust",
        ] {
            assert!(
                !question_seeks_code(q),
                "embedded trigger must not fire: {q}"
            );
        }
    }

    // Measured live miss (2026-07-31): this exact question was rejected and the
    // agent investigated blind for 16 rounds. The mechanism COLLOCATION admits
    // it; the bare word must not, or HR policy questions start injecting code.
    #[test]
    fn policy_mechanism_collocations_fire_but_bare_policy_does_not() {
        assert!(question_seeks_code(
            "Is there any way a tool can run without policy evaluation?"
        ));
        assert!(question_seeks_code(
            "where does the policy engine load rules from"
        ));
        assert!(
            !question_seeks_code("what is the annual leave policy for the trust?"),
            "bare 'policy' is shared vocabulary and must not inject"
        );
        assert!(
            !question_seeks_code("did the escalation policy change last week?"),
            "ops policy questions stay out"
        );
    }

    #[test]
    fn word_bounded_matching_still_fires_on_real_phrases() {
        assert!(contains_word_bounded(
            "how does the scheduler work",
            "how does"
        ));
        assert!(!contains_word_bounded("the show doesn't start", "how does"));
        assert!(contains_word_bounded("what is this", "what"));
        assert!(!contains_word_bounded("whatsapp message", "what"));
    }

    // The grounding line is the deterministic confirmation surfaced inside the
    // answer on chip-less surfaces (Slack): compact, capped, commit-stamped.
    #[test]
    fn grounding_line_is_compact_capped_and_commit_stamped() {
        let mk = |i: usize| CodeCitation {
            repo: "workforce".into(),
            owner: "driftwerk".into(),
            repo_name: "workforce".into(),
            commit: "abcdef1234567890".into(),
            path: format!("crates/hub/src/file{i}.rs"),
            start_line: 10 + i,
            end_line: 20 + i,
            kind: "function".into(),
            symbol: format!("f{i}"),
        };
        let line = grounding_line(&(0..10).map(mk).collect::<Vec<_>>());
        assert!(line.starts_with("\n\n_Grounded in "), "line: {line}");
        assert!(line.contains("`crates/hub/src/file0.rs:10`"));
        assert!(line.contains("+2 more"), "10 citations, 8 shown: {line}");
        assert!(line.contains("@ abcdef123"), "short commit stamp: {line}");
        assert!(grounding_line(&[]).is_empty(), "no citations, no line");
    }

    // The softened header carries an explicit escape hatch so the model may
    // disregard an irrelevant block rather than reconcile with it.
    #[test]
    fn header_offers_escape_hatch() {
        assert!(INJECTION_HEADER.contains("do not force an answer from unrelated code"));
    }
}

//! One-shot in-place groomer for the *stored* lesson corpus.
//!
//! Where [`crate::lessons_backfill`] re-extracts lessons from historical
//! sessions, this walks the lessons already in the [`MemoryStore`] and cleans
//! them up after the fact:
//!
//! * **Prune** — re-applies the live extractor's "concrete system-specific
//!   anchor" bar (content-only, since the original turn context isn't stored)
//!   and drops generic assistant-etiquette platitudes that slipped in before
//!   the prompt was tightened.
//! * **Merge** — collapses genuine near-duplicate re-extractions that piled up
//!   while write-side dedup was mis-calibrated, using the same similarity bar
//!   the live path now uses.
//!
//! `forget()` is irreversible, so callers should run with `dry_run` +
//! `print` first and eyeball the decisions before committing.

use std::collections::HashSet;
use std::sync::Arc;

use eyre::Result;
use serde::Deserialize;
use tracing::{info, warn};

use workforce_agent::compaction::extract_text_from_response;
use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::llm::types::LlmMessage;
use workforce_core::types::AgentId;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{Memory, MemoryFilter, MemorySource};

use crate::turn_extraction::{strip_code_fence, LESSON_NEAR_DUPLICATE_SIMILARITY};

/// Upper bound on lessons pulled for a single groom pass. Far above any real
/// per-agent corpus; exists only so `list`'s small default page doesn't
/// silently truncate the scan.
const MAX_CORPUS: usize = 10_000;

const PRUNE_PROMPT: &str = "\
You are grooming an agent's lesson book. Decide whether to KEEP or DROP one \
stored lesson, judging only its text.

DROP it when the lesson is generic assistant etiquette or a platitude that any \
competent assistant already follows and that names no concrete, system-specific \
anchor — e.g. 'acknowledge the user's correction', 'explain instead of changing \
things when asked how it works', 'act instead of describing intent', 'trust the \
user's stated facts over your assumptions'.

KEEP it only when it names a concrete, system-specific anchor — a specific tool, \
skill, config key, selector, error string, service, or API behaviour — that a \
future agent could not derive from generic good manners.

Respond with ONLY a JSON object, no prose or markdown:
  {\"keep\": <true|false>, \"reason\": \"<short>\"}";

/// Caller-supplied knobs for a cleanup run.
#[derive(Debug, Clone)]
pub struct CleanupOptions {
    /// Agent IDs to groom. Empty means every owner found in the lesson corpus.
    pub agent_ids: Vec<AgentId>,
    /// When `true`, decisions are computed and reported but the store is never
    /// mutated — no `forget`, no `record_recurrence`.
    pub dry_run: bool,
    /// Optional override for the prune-classifier model.
    pub model: Option<String>,
    /// Recall-certainty bar above which two lessons count as merge-worthy
    /// near-duplicates. Defaults to [`LESSON_NEAR_DUPLICATE_SIMILARITY`].
    pub merge_similarity: f32,
    /// When `true`, the report carries a per-lesson decision list for review.
    pub print: bool,
}

impl Default for CleanupOptions {
    fn default() -> Self {
        Self {
            agent_ids: Vec::new(),
            dry_run: true,
            model: None,
            merge_similarity: LESSON_NEAR_DUPLICATE_SIMILARITY,
            print: false,
        }
    }
}

/// Per-run aggregate counters.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CleanupReport {
    pub lessons_scanned: usize,
    pub pruned: usize,
    pub merged: usize,
    pub kept: usize,
    pub errors: usize,
    /// Populated only when [`CleanupOptions::print`] is set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<CleanupDecision>,
}

/// One groom decision, captured for review when `print` is set.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CleanupDecision {
    pub owner: String,
    /// `"prune"`, `"merge"`, or `"keep"`.
    pub action: &'static str,
    pub content: String,
    /// Prune reason, or the keeper's content for a merge.
    pub detail: String,
}

#[derive(Debug, Deserialize)]
struct PruneVerdict {
    keep: bool,
    #[serde(default)]
    reason: String,
}

/// Ask the classifier whether to keep a lesson. Defaults to keeping on any LLM
/// or parse failure — pruning is destructive, so errors must never drop a
/// lesson.
async fn classify_keep(llm: &dyn LlmProvider, content: &str, model: Option<&str>) -> PruneVerdict {
    let keep = |reason: &str| PruneVerdict {
        keep: true,
        reason: reason.to_string(),
    };
    // Reasoning suppressed: a keep/prune verdict needs no chain of thought, and
    // reasoning models otherwise consume the whole budget on hidden tokens and
    // return empty content.
    let response =
        match workforce_agent::llm::provider::with_reasoning_suppressed(llm.create_message(
            Some(PRUNE_PROMPT),
            &[LlmMessage::user(content.to_string())],
            &[],
            model,
            Some(2048),
            true,
        ))
        .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "Prune classifier failed; keeping lesson");
                return keep("classifier error");
            }
        };
    let raw = extract_text_from_response(&response.content);
    match serde_json::from_str::<PruneVerdict>(strip_code_fence(raw.trim()).trim()) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "Prune verdict unparseable; keeping lesson");
            keep("unparseable verdict")
        }
    }
}

/// Groom the stored lesson corpus in place.
pub async fn run(
    llm: Arc<dyn LlmProvider>,
    store: Arc<dyn MemoryStore>,
    opts: CleanupOptions,
) -> Result<CleanupReport> {
    let filter = MemoryFilter {
        source: Some(MemorySource::Lesson),
        system_unscoped: true,
        // Explicit high limit: `list` defaults to a small page (10), which
        // would silently groom only a fraction of the corpus.
        limit: Some(MAX_CORPUS),
        ..Default::default()
    };
    let mut lessons: Vec<Memory> = store.list(filter).await?;
    if lessons.len() >= MAX_CORPUS {
        warn!(
            cap = MAX_CORPUS,
            "Lesson corpus hit the scan cap; some lessons were not groomed this run"
        );
    }
    if !opts.agent_ids.is_empty() {
        lessons.retain(|m| opts.agent_ids.contains(&m.owner));
    }

    let mut report = CleanupReport {
        lessons_scanned: lessons.len(),
        ..Default::default()
    };

    // ── Prune pass ────────────────────────────────────────────────────
    let mut removed: HashSet<String> = HashSet::new();
    for lesson in &lessons {
        let verdict = classify_keep(llm.as_ref(), &lesson.content, opts.model.as_deref()).await;
        if verdict.keep {
            continue;
        }
        report.pruned += 1;
        removed.insert(lesson.id.0.clone());
        if opts.print {
            report.decisions.push(CleanupDecision {
                owner: lesson.owner.as_str().to_string(),
                action: "prune",
                content: lesson.content.clone(),
                detail: verdict.reason,
            });
        }
        if !opts.dry_run {
            if let Err(e) = store.forget(&lesson.id).await {
                report.errors += 1;
                warn!(error = %e, id = %lesson.id.0, "Failed to forget pruned lesson");
                removed.remove(&lesson.id.0);
                report.pruned -= 1;
            }
        }
    }

    // ── Merge pass ────────────────────────────────────────────────────
    // Highest-importance survivor in each near-duplicate cluster is the
    // keeper; lower ones merge into it. Processing importance-first keeps the
    // most-reinforced phrasing.
    let scanned_ids: HashSet<String> = lessons.iter().map(|m| m.id.0.clone()).collect();
    let mut survivors: Vec<&Memory> = lessons
        .iter()
        .filter(|m| !removed.contains(&m.id.0))
        .collect();
    survivors.sort_by(|a, b| {
        b.importance
            .partial_cmp(&a.importance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for keeper in &survivors {
        if removed.contains(&keeper.id.0) {
            continue;
        }
        let dup_filter = MemoryFilter {
            source: Some(MemorySource::Lesson),
            owner: Some(keeper.owner.clone()),
            system_unscoped: true,
            limit: Some(10),
            ..Default::default()
        };
        let candidates = match store.recall(&keeper.content, dup_filter).await {
            Ok(c) => c,
            Err(e) => {
                report.errors += 1;
                warn!(error = %e, "Merge recall failed");
                continue;
            }
        };
        for cand in candidates {
            let id = &cand.memory.id.0;
            if cand.score < opts.merge_similarity
                || *id == keeper.id.0
                || removed.contains(id)
                || !scanned_ids.contains(id)
            {
                continue;
            }
            report.merged += 1;
            removed.insert(id.clone());
            if opts.print {
                report.decisions.push(CleanupDecision {
                    owner: cand.memory.owner.as_str().to_string(),
                    action: "merge",
                    content: cand.memory.content.clone(),
                    detail: keeper.content.clone(),
                });
            }
            if !opts.dry_run {
                // Record provenance on the keeper first; only forget the
                // duplicate if that succeeded, so a failure never deletes the
                // lesson without folding its source ids into the survivor.
                let merge = async {
                    store
                        .record_recurrence(&keeper.id, cand.memory.source_message_ids.clone())
                        .await?;
                    store.forget(&cand.memory.id).await
                }
                .await;
                if let Err(e) = merge {
                    report.errors += 1;
                    warn!(error = %e, id = %id, "Failed to merge duplicate lesson");
                    removed.remove(id);
                    report.merged -= 1;
                }
            }
        }
    }

    report.kept = report
        .lessons_scanned
        .saturating_sub(report.pruned + report.merged);
    Ok(report)
}

/// Format a cleanup report for stdout.
pub fn format_report(report: &CleanupReport) -> String {
    let mut out = format!(
        "lessons scanned        : {}\n\
         pruned (generic)       : {}\n\
         merged (near-dup)      : {}\n\
         kept                   : {}\n\
         errors                 : {}",
        report.lessons_scanned, report.pruned, report.merged, report.kept, report.errors,
    );
    if !report.decisions.is_empty() {
        out.push_str("\n\n── Decisions ───────────────────────────────────────────────");
        for d in &report.decisions {
            out.push_str(&format!("\n\n  [{}] {} — {}", d.owner, d.action, d.content));
            match d.action {
                "prune" => out.push_str(&format!("\n     reason: {}", d.detail)),
                "merge" => out.push_str(&format!("\n     into: {}", d.detail)),
                _ => {}
            }
        }
    }
    out
}

/// Log a one-line `info!` summary.
pub fn log_report_summary(report: &CleanupReport) {
    info!(
        scanned = report.lessons_scanned,
        pruned = report.pruned,
        merged = report.merged,
        kept = report.kept,
        errors = report.errors,
        "Lessons cleanup complete"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use workforce_agent::llm::streaming::StreamEvent;
    use workforce_agent::llm::types::{
        ContentBlock, LlmResponse, LlmToolDef, StopReason, TokenUsage,
    };
    use workforce_memory::in_memory::InMemoryStore;
    use workforce_memory::types::{Memory, MemoryLayer};

    /// Mock that keeps lessons whose text contains "CONCRETE" and drops the
    /// rest — lets the prune pass be exercised deterministically.
    struct AnchorLlm;

    #[async_trait]
    impl LlmProvider for AnchorLlm {
        async fn create_message(
            &self,
            _system: Option<&str>,
            messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _cache: bool,
        ) -> Result<LlmResponse> {
            let text = format!("{:?}", messages);
            let keep = text.contains("CONCRETE");
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: format!("{{\"keep\": {keep}, \"reason\": \"test\"}}"),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: "mock".to_string(),
            })
        }

        async fn create_message_stream(
            &self,
            _system: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _cache: bool,
            _tx: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            unreachable!("cleanup uses create_message only")
        }
    }

    fn lesson(content: &str, importance: f32) -> Memory {
        Memory::new(content, MemoryLayer::Semantic, AgentId::new("allan"))
            .with_source(MemorySource::Lesson)
            .with_identity_id("u1")
            .with_importance(importance)
    }

    #[test]
    fn parse_prune_verdict_reads_keep_and_reason() {
        let v: PruneVerdict =
            serde_json::from_str(r#"{"keep": false, "reason": "generic etiquette"}"#).unwrap();
        assert!(!v.keep);
        assert_eq!(v.reason, "generic etiquette");
    }

    #[tokio::test]
    async fn dry_run_reports_without_mutating() {
        let store = Arc::new(InMemoryStore::new());
        store
            .remember(lesson("When the user corrects you, apologise.", 0.7))
            .await
            .unwrap();
        store
            .remember(lesson(
                "When CONCRETE: use the github skill, not raw vault creds.",
                0.8,
            ))
            .await
            .unwrap();

        let report = run(
            Arc::new(AnchorLlm),
            store.clone(),
            CleanupOptions {
                dry_run: true,
                print: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(report.lessons_scanned, 2);
        assert_eq!(report.pruned, 1, "the etiquette lesson is flagged");
        assert_eq!(report.kept, 1);
        // Dry run leaves the store untouched.
        let filter = MemoryFilter {
            source: Some(MemorySource::Lesson),
            system_unscoped: true,
            ..Default::default()
        };
        assert_eq!(store.list(filter).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn commit_prunes_generic_and_merges_duplicates() {
        let store = Arc::new(InMemoryStore::new());
        // One generic (pruned), two concrete near-identical (one merges away).
        store
            .remember(lesson(
                "When the user corrects you, apologise and move on.",
                0.6,
            ))
            .await
            .unwrap();
        store.remember(lesson("When CONCRETE keycloak login, click the kc-login selector after filling the password.", 0.85)).await.unwrap();
        store.remember(lesson("When CONCRETE keycloak login, click the kc-login selector after filling the password field.", 0.7)).await.unwrap();

        let report = run(
            Arc::new(AnchorLlm),
            store.clone(),
            CleanupOptions {
                dry_run: false,
                merge_similarity: 0.8, // keyword overlap of the two concrete lessons clears this
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(report.lessons_scanned, 3);
        assert_eq!(report.pruned, 1);
        assert_eq!(report.merged, 1);
        assert_eq!(report.kept, 1);
        let filter = MemoryFilter {
            source: Some(MemorySource::Lesson),
            system_unscoped: true,
            ..Default::default()
        };
        let remaining = store.list(filter).await.unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "only the highest-importance concrete lesson survives"
        );
        assert!((remaining[0].importance - 0.85).abs() < 1e-6);
    }
}

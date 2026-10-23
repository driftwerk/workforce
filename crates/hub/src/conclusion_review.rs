//! Conclusion review — an automatic adversarial review of a consequential answer.
//!
//! Claim grounding catches confident wrong *claims*; this catches a wrong or
//! incomplete *conclusion* — the ENG-104 failure where an answer under-scopes or
//! mis-diagnoses. An independent reviewer critiques the answer against source and
//! surfaces substantive problems. Cold-tested high-precision / low-noise; recall
//! is partial, so it reports the problems it finds and NEVER asserts completeness
//! — a quiet review is not a clean bill of health.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::Tool;

use crate::agent_turn::ToolLoop;
use crate::claim_verifier::{parse_json_array, run_grounding_loop};

/// One substantive problem the reviewer surfaced, grounded in source.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewFinding {
    pub issue: String,
    #[serde(default)]
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewReport {
    pub findings: Vec<ReviewFinding>,
}

impl ReviewReport {
    /// Render the surfaced problems. `None` when there are none — which means "no
    /// issues surfaced", never "verified complete" (recall is partial by design).
    pub fn render(&self) -> Option<String> {
        if self.findings.is_empty() {
            return None;
        }
        let total = self.findings.len();
        let plural = if total == 1 { "" } else { "s" };
        let mut out =
            format!("⚠ Review found {total} substantive issue{plural} with this answer:\n");
        for finding in self.findings.iter().take(MAX_FINDINGS_RENDERED) {
            if finding.evidence.is_empty() {
                out.push_str(&format!("- {}\n", finding.issue));
            } else {
                out.push_str(&format!("- {}: {}\n", finding.issue, finding.evidence));
            }
        }
        if total > MAX_FINDINGS_RENDERED {
            out.push_str(&format!("- …and {} more\n", total - MAX_FINDINGS_RENDERED));
        }
        Some(out)
    }
}

#[async_trait]
pub trait ConclusionReviewer: Send + Sync {
    async fn review(&self, request: &str, answer: &str) -> Result<Vec<ReviewFinding>>;
}

/// Runs the review and assembles the report. The reviewer is injected so the
/// engine is testable without a live model + source tools.
pub struct ConclusionReviewRunner {
    reviewer: Arc<dyn ConclusionReviewer>,
}

impl ConclusionReviewRunner {
    pub fn new(reviewer: Arc<dyn ConclusionReviewer>) -> Self {
        Self { reviewer }
    }

    pub async fn run(&self, request: &str, answer: &str) -> Result<ReviewReport> {
        let findings = self.reviewer.review(request, answer).await?;
        tracing::info!(target: "conclusion_review", n_findings = findings.len(), "conclusion review");
        Ok(ReviewReport { findings })
    }
}

/// Correctness lens: wrong claims / wrong diagnosis in the answer's code/system
/// substance.
const CORRECTNESS_LENS: &str = "You are an adversarial reviewer of the CODE/SYSTEM substance of an \
    agent's ANSWER, checked against the real source with your read tools (grep/read the project). You \
    can ONLY verify claims about the codebase/system. If the answer is grounded in data you cannot \
    access — the user's email/calendar/files, external APIs or services, runtime/operational state, \
    or the agent's own tool results — you CANNOT verify it: do NOT flag it, and NEVER call it a \
    hallucination merely because it is absent from the source code (absence from the code is EXPECTED \
    for non-code data). Do NOT assume which tools the agent has, and never claim 'the agent can't do \
    X'. Report ONLY SUBSTANTIVE code/system problems a human acting on the answer would be hurt by — \
    a missed major path or category, a wrong claim about the code, a wrong diagnosis — each grounded \
    with file:line evidence. Look BEYOND the obvious layer: background jobs, model callbacks/hooks, \
    overrides, and paths OUTSIDE the main module are where misses hide. Do NOT report nitpicks, \
    style, scope the question did not ask for, or \"you might also consider\" boilerplate. Do NOT \
    claim the answer is complete or all-clear. Return ONLY a JSON array of \
    [{\"issue\": <string>, \"evidence\": <file:line or short quote>}]; return an empty array [] when \
    you find no substantive code/system problems. No prose, no markdown fences.";

/// Completeness lens: whole capabilities/categories the answer OMITTED that the
/// source clearly has — the under-scoping a correctness pass alone misses.
const COMPLETENESS_LENS: &str = "You are a COMPLETENESS reviewer of the CODE/SYSTEM substance of an \
    agent's ANSWER, checked against the real source with your read tools (grep/read; the module/tool \
    listing in source). Your ONLY job: find MAJOR capabilities or feature categories the source \
    clearly has that the answer OMITTED entirely — whole missing subsystems, not the wording of what \
    is listed. Walk the source's module/tool listing before judging. Report the MOST SIGNIFICANT \
    omissions, each named with its implementing module/dir as evidence. Flag an omission ONLY if it \
    is a WHOLE major subsystem a knowledgeable reader would call a real gap; if the answer already \
    covers the major areas, return an empty array even when minor or sub-component features go \
    unlisted. You can ONLY judge \
    code/system completeness: if the answer is grounded in data you cannot access — the user's \
    email/calendar/files, external APIs, runtime state, or the agent's own tool results — you CANNOT \
    judge its completeness, so do NOT flag it. Do NOT nitpick the wording of what IS listed, or \
    report scope the question did not ask for. Return ONLY a JSON array of \
    [{\"issue\": <missing capability>, \"evidence\": <module/dir>}]; an empty array [] when nothing \
    major is missing. No prose, no markdown fences.";

/// Lenses run in parallel; their findings are unioned. Correctness catches wrong
/// claims; completeness catches under-scoping (each is blind to the other's class).
const LENSES: &[&str] = &[CORRECTNESS_LENS, COMPLETENESS_LENS];

/// Correctness lens alone — for callers (e.g. knowledge-node review) that want
/// wrong-claim detection without the "main features"-shaped completeness pass.
const CORRECTNESS_ONLY: &[&str] = &[CORRECTNESS_LENS];

/// Cap on findings rendered into the block; the count header still reports the
/// true total, with a "…and N more" line when it overflows.
const MAX_FINDINGS_RENDERED: usize = 8;

/// Live reviewer: runs each lens as its own advisor tool loop with the agent's raw
/// `repo.grep`/`repo.read` tools, in parallel, and unions their findings.
pub struct MultiLensConclusionReviewer {
    tool_loop: Arc<dyn ToolLoop>,
    tools: Vec<Arc<dyn Tool>>,
    model: Option<String>,
    lenses: &'static [&'static str],
    cancel: Option<tokio_util::sync::CancellationToken>,
}

impl MultiLensConclusionReviewer {
    /// All lenses — correctness + completeness (catches under-scoping).
    pub fn new(
        tool_loop: Arc<dyn ToolLoop>,
        tools: Vec<Arc<dyn Tool>>,
        model: Option<String>,
    ) -> Self {
        Self {
            tool_loop,
            tools,
            model,
            lenses: LENSES,
            cancel: None,
        }
    }

    /// Stop every lens when the turn the review annotates is stopped.
    pub fn with_cancel(mut self, cancel: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Correctness lens only — no completeness pass.
    pub fn correctness_only(
        tool_loop: Arc<dyn ToolLoop>,
        tools: Vec<Arc<dyn Tool>>,
        model: Option<String>,
    ) -> Self {
        Self {
            tool_loop,
            tools,
            model,
            lenses: CORRECTNESS_ONLY,
            cancel: None,
        }
    }
}

#[async_trait]
impl ConclusionReviewer for MultiLensConclusionReviewer {
    async fn review(&self, request: &str, answer: &str) -> Result<Vec<ReviewFinding>> {
        let user_message = format!(
            "ORIGINAL QUESTION:\n{request}\n\nAGENT'S ANSWER TO REVIEW (against the current \
             project's source):\n{answer}"
        );
        let legs = self.lenses.iter().map(|&lens| {
            run_grounding_loop(
                self.tool_loop.as_ref(),
                &self.tools,
                self.model.as_deref(),
                lens,
                user_message.clone(),
                "conclusion",
                self.cancel.clone(),
                None,
                parse_json_array::<ReviewFinding>,
            )
        });
        // The conclusion lenses do not report coverage: they run without a
        // research deadline, so a lens either produced findings or errored.
        let per_lens: Vec<Result<Vec<ReviewFinding>>> = futures::future::join_all(legs)
            .await
            .into_iter()
            .map(|r| r.map(|(findings, _complete)| findings))
            .collect();
        merge_lens_results(per_lens)
    }
}

/// Union each lens's findings, skipping (and logging) a lens that errored — a
/// failed lens must not sink the others — then drop near-identical findings the
/// lenses may both surface, keeping the first. When every lens failed there
/// was no review, which is an error, not a quiet one.
fn merge_lens_results(results: Vec<Result<Vec<ReviewFinding>>>) -> Result<Vec<ReviewFinding>> {
    let mut findings = Vec::new();
    let mut reviewed = false;
    let mut last_error = None;
    for (lens_idx, result) in results.into_iter().enumerate() {
        match result {
            Ok(lens_findings) => {
                reviewed = true;
                findings.extend(lens_findings);
            }
            Err(e) => {
                tracing::warn!(target: "conclusion_review", lens_idx, error = %e, "review lens failed");
                last_error = Some(e);
            }
        }
    }
    if let (false, Some(e)) = (reviewed, last_error) {
        return Err(e.wrap_err("every review lens failed"));
    }
    let mut seen = std::collections::HashSet::new();
    Ok(findings
        .into_iter()
        .filter(|finding| seen.insert(finding.issue.to_lowercase()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(issue: &str) -> ReviewFinding {
        ReviewFinding {
            issue: issue.to_string(),
            evidence: "f.rb:1".to_string(),
        }
    }

    #[test]
    fn render_is_none_when_no_findings_and_lists_them_otherwise() {
        assert!(ReviewReport { findings: vec![] }.render().is_none());

        let rendered = ReviewReport {
            findings: vec![
                finding("missed CalculateShiftStatusJob"),
                ReviewFinding {
                    issue: "wrong: CancelWorker fires".to_string(),
                    evidence: String::new(),
                },
            ],
        }
        .render()
        .unwrap();
        assert!(rendered.contains("Review found 2 substantive issues"));
        assert!(rendered.contains("missed CalculateShiftStatusJob: f.rb:1"));
        assert!(rendered.contains("- wrong: CancelWorker fires\n"));
        assert!(!rendered.to_lowercase().contains("complete"));
    }

    #[test]
    fn render_singular_for_one_finding() {
        let rendered = ReviewReport {
            findings: vec![finding("one issue")],
        }
        .render()
        .unwrap();
        assert!(rendered.contains("1 substantive issue with"));
        assert!(!rendered.contains("issues"));
    }

    #[test]
    fn parse_findings_tolerates_fences_and_a_bad_element() {
        let findings: Vec<ReviewFinding> = parse_json_array(
            "```json\n[{\"issue\":\"a\",\"evidence\":\"x.rb:9\"},{\"nope\":1},\
             {\"issue\":\"b\"}]\n```",
        )
        .unwrap();
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].evidence, "x.rb:9");
        assert_eq!(findings[1].evidence, "");
    }

    #[test]
    fn merge_unions_lenses_skips_errors_and_dedups() {
        let merged = merge_lens_results(vec![
            Ok(vec![finding("Missed coding"), finding("wrong claim")]),
            Err(eyre::eyre!("a lens failed")),
            Ok(vec![finding("missed CODING"), finding("missed vault")]),
        ])
        .unwrap();
        // the errored lens is skipped; "Missed coding"/"missed CODING" dedup.
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].issue, "Missed coding");
        assert_eq!(merged[1].issue, "wrong claim");
        assert_eq!(merged[2].issue, "missed vault");
    }

    /// The fail-open regression: lenses that all failed reviewed nothing, and
    /// must not read as a review that found nothing.
    #[test]
    fn every_lens_failing_is_an_error_not_a_quiet_review() {
        assert!(merge_lens_results(vec![
            Err(eyre::eyre!("timed out")),
            Err(eyre::eyre!("provider error")),
        ])
        .is_err());
        assert!(merge_lens_results(vec![Err(eyre::eyre!("x")), Ok(vec![])])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn render_caps_findings_but_reports_true_total() {
        let findings: Vec<ReviewFinding> = (0..MAX_FINDINGS_RENDERED + 3)
            .map(|i| finding(&format!("issue {i}")))
            .collect();
        let rendered = ReviewReport { findings }.render().unwrap();
        assert!(rendered.contains(&format!("found {} substantive", MAX_FINDINGS_RENDERED + 3)));
        assert!(rendered.contains("issue 0"));
        assert!(rendered.contains(&format!("issue {}", MAX_FINDINGS_RENDERED - 1)));
        assert!(!rendered.contains(&format!("issue {}", MAX_FINDINGS_RENDERED)));
        assert!(rendered.contains("…and 3 more"));
    }

    struct FixedReviewer(Vec<ReviewFinding>);
    #[async_trait]
    impl ConclusionReviewer for FixedReviewer {
        async fn review(&self, _request: &str, _answer: &str) -> Result<Vec<ReviewFinding>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn runner_renders_findings_and_is_silent_when_none() {
        let runner = ConclusionReviewRunner::new(Arc::new(FixedReviewer(vec![finding("x")])));
        assert!(runner
            .run("q", "a")
            .await
            .unwrap()
            .render()
            .unwrap()
            .contains("x"));

        let empty = ConclusionReviewRunner::new(Arc::new(FixedReviewer(vec![])));
        assert!(empty.run("q", "a").await.unwrap().render().is_none());
    }
}

//! The quality gate deep expertise passes before it is kept: claim grounding,
//! conclusion review, and the notes they leave on the service.

use std::sync::Arc;

use crate::serve::{AgentService, CLAIM_GROUNDING_TIMEOUT_SECS};

/// Post-turn quality gate for a deep-expertise node — the point of the mode.
/// Grounds the node's body against the project source: a CLEAN grounding
/// promotes the node to `Verified`; otherwise it stays `Modelled` and the
/// unverified claims are appended to its `review_note`. Then an adversarial
/// conclusion review turns each under-scoping finding into an uncovered
/// coverage gap, so the next deepening turn closes it.
///
/// Best-effort and fail-safe: with no source-read tools the grounder cannot
/// run, so `Verified` is never granted (it is earned by a clean grounding) —
/// the node is left `Modelled`. The dirty path is a single read-modify-write
/// (`set_status` writes only status/review columns and would clobber the
/// coverage `upsert_node` writes, so both mutations go through one node row).
pub(crate) async fn deep_expertise_quality_gate(
    service: &Arc<AgentService>,
    store: &dyn workforce_self_learning::DeepExpertiseStore,
    node: workforce_self_learning::DeepNode,
    reviewer_id: &str,
) {
    let Some(tools) = service.source_review_tools() else {
        tracing::info!(
            target: "deep_expertise",
            agent = %service.agent_id,
            node = %node.title,
            "quality gate skipped: no source-read tools; node left modelled",
        );
        return;
    };

    let grounding_model = service.resolved.snapshot().claim_grounding.model.clone();
    let grounding_loop = crate::turn_loop::grounding_loop(service).await;
    let grounder = crate::claim_verifier::ClaimGrounder::new(Arc::new(
        crate::claim_verifier::AdvisorClaimChecker::new(
            Arc::clone(&grounding_loop),
            tools.clone(),
            grounding_model.clone(),
        ),
    ));
    let grounding = tokio::time::timeout(
        std::time::Duration::from_secs(CLAIM_GROUNDING_TIMEOUT_SECS),
        grounder.ground(&node.title, &node.body),
    )
    .await;

    match grounding {
        Ok(Ok(report)) if report.is_clean() => {
            if let Err(e) =
                store.set_status(node.id, workforce_self_learning::NodeStatus::Verified, None)
            {
                tracing::warn!(target: "deep_expertise", agent = %service.agent_id, node = %node.title, error = %e, "failed to mark node verified");
            } else {
                tracing::info!(target: "deep_expertise", agent = %service.agent_id, node = %node.title, "node grounded clean → verified");
            }
        }
        Ok(Ok(report)) => {
            let unverified: Vec<String> = report
                .claims
                .iter()
                .filter(|c| c.tier != crate::claim_verifier::Tier::Verified && !c.text.is_empty())
                .map(|c| c.text.clone())
                .collect();
            tracing::info!(target: "deep_expertise", agent = %service.agent_id, node = %node.title, n_claims = report.claims.len(), n_unverified = unverified.len(), complete = report.complete, "node not clean → left modelled");
            apply_deep_review_note(service, store, &node, &unverified, reviewer_id);
        }
        Ok(Err(e)) => {
            tracing::warn!(target: "deep_expertise", agent = %service.agent_id, node = %node.title, error = %e, "claim grounding failed; node left modelled");
        }
        Err(_) => {
            tracing::warn!(target: "deep_expertise", agent = %service.agent_id, node = %node.title, timeout_secs = CLAIM_GROUNDING_TIMEOUT_SECS, "claim grounding timed out; node left modelled");
        }
    }

    // Conclusion review runs regardless of the grounding verdict: an
    // under-scoping finding is a coverage gap the next turn must close, even
    // for a node whose recorded claims all grounded clean.
    let runner = crate::conclusion_review::ConclusionReviewRunner::new(Arc::new(
        crate::conclusion_review::MultiLensConclusionReviewer::correctness_only(
            grounding_loop,
            tools,
            grounding_model,
        ),
    ));
    let review = tokio::time::timeout(
        std::time::Duration::from_secs(CLAIM_GROUNDING_TIMEOUT_SECS),
        runner.run(&node.title, &node.body),
    )
    .await;
    match review {
        Ok(Ok(report)) if !report.findings.is_empty() => {
            let gaps: Vec<String> = report.findings.into_iter().map(|f| f.issue).collect();
            apply_deep_coverage_gaps(service, store, node.id, &gaps);
        }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            tracing::warn!(target: "deep_expertise", agent = %service.agent_id, node = %node.title, error = %e, "conclusion review failed");
        }
        Err(_) => {
            tracing::warn!(target: "deep_expertise", agent = %service.agent_id, node = %node.title, timeout_secs = CLAIM_GROUNDING_TIMEOUT_SECS, "conclusion review timed out");
        }
    }
}

/// Append the grounding's unverified claims to a node's `review_note` via a
/// single read-modify-write, keeping it `Modelled`. Re-reads the node so the
/// write does not clobber a concurrent coverage update.
pub(crate) fn apply_deep_review_note(
    service: &AgentService,
    store: &dyn workforce_self_learning::DeepExpertiseStore,
    node: &workforce_self_learning::DeepNode,
    unverified: &[String],
    reviewer_id: &str,
) {
    if unverified.is_empty() {
        return;
    }
    let Ok(Some(mut current)) = store.get_node(node.id) else {
        return;
    };
    let appended = format!(
        "{} claim(s) could not be verified against source:\n{}",
        unverified.len(),
        unverified
            .iter()
            .map(|c| format!("• {c}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    current.review_note = Some(match current.review_note.take() {
        Some(prev) if !prev.trim().is_empty() => format!("{prev}\n{appended}"),
        _ => appended,
    });
    current.reviewed_at = Some(chrono::Utc::now());
    current.reviewed_by = Some(reviewer_id.to_string());
    if let Err(e) = store.upsert_node(&current) {
        tracing::warn!(target: "deep_expertise", agent = %service.agent_id, node = %current.title, error = %e, "failed to append review note");
    }
}

/// Merge reviewer findings into a node's coverage as uncovered gaps via a
/// single read-modify-write, so the next deepening turn targets them.
pub(crate) fn apply_deep_coverage_gaps(
    service: &AgentService,
    store: &dyn workforce_self_learning::DeepExpertiseStore,
    node_id: uuid::Uuid,
    gaps: &[String],
) {
    let Ok(Some(mut current)) = store.get_node(node_id) else {
        return;
    };
    let merged = crate::self_learning::deep::merge_findings_into_coverage(&current.coverage, gaps);
    if merged.len() == current.coverage.len() {
        return;
    }
    current.coverage = merged;
    if let Err(e) = store.upsert_node(&current) {
        tracing::warn!(target: "deep_expertise", agent = %service.agent_id, node = %current.title, error = %e, "failed to append coverage gaps");
    }
}

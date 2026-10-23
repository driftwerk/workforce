//! How a turn's claims are grounded, and what the outcome says.

use crate::serve::is_workflow_step_session;

/// Whether the post-answer review footers — claim grounding, data grounding,
/// failed-tool notices, commitment caveats — may run for this turn. Spoken
/// turns never qualify: the blocks are written to be read, they land after the
/// answer has settled, and every voice surface speaks stream text verbatim —
/// iOS and the voice bridge read "unverified claims" footnotes aloud. Per
/// *turn* like [`code_context_eligible`], so typed turns in a shared iOS
/// session keep their reviews. Workflow steps never qualify either: their
/// responses are parsed by the OUTPUTS extractor, not read by a human, and
/// an enabled LLM review would block step advancement.
pub(crate) fn post_answer_reviews_eligible(session: &workforce_common::session::Session) -> bool {
    !session.is_voice_turn() && !is_workflow_step_session(session)
}

/// Wall-clock bound on DATA-claim grounding, which is deliberately NOT the
/// configurable reply-review budget above.
///
/// That budget is safe to enlarge because the loop it bounds splits into
/// research and a reserved write-up, so a long pass NARROWS — it reports what it
/// reached instead of vanishing. This path has no such split: it is one
/// `ground()` call under a single timeout, so the only thing a larger bound buys
/// is more work discarded whole on expiry. That is the 180s-era shape the split
/// exists to prevent, so this stays where it was.
pub(crate) const DATA_CLAIM_REVIEW_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(40);

/// What a source-grounding pass concluded. "Clean" here means nothing was
/// contradicted; [`crate::claim_verifier::GroundingReport::is_clean`] is the
/// stricter "everything verified" the deep-expertise gate needs.
///
/// `Option<String>` could not carry this: it collapsed "never ran", "checked and
/// clean" and "started and gave up" into one silence. Once the reader is told
/// their answer is being checked, that silence is the worst possible answer —
/// they cannot tell a pass from a failure, and the honest reading of "no news"
/// becomes "it was verified", which is exactly what did not happen.
pub(crate) enum GroundingOutcome {
    /// Never started: disabled, answer too short, or no source-read tools. The
    /// reader is not told a check is happening either, so there is nothing to
    /// report.
    NotRun,
    /// Ran to completion, checked claims and contradicted none. Silent by
    /// design — an ordinary answer grows no footer, and the turn's completion
    /// tick is the confirmation.
    Clean,
    /// Returned a verdict holding no claims at all. Legitimate for a non-code
    /// answer, but it verified nothing, so it is never counted as `Clean` —
    /// that conflation is how a day of empty verdicts read as a day of passes.
    NothingChecked,
    /// Ran to completion and contradicted nothing, but left some claims
    /// unverified — not a pass either.
    Unsettled,
    /// Ran out of research time and wrote up what it had. Nothing it REACHED
    /// was contradicted, but the answer was not covered end to end — reporting
    /// that as clean would claim more verification than happened.
    PartiallyChecked,
    /// Ran and found something. The rendered ⚠ block.
    Flagged(String),
    /// Started and produced no verdict. MUST be reported: the reader watched us
    /// say we were checking, and is otherwise left believing we did.
    Incomplete,
    /// The reader stopped the turn. Not a failure, and nobody is waiting.
    Cancelled,
}

impl GroundingOutcome {
    /// The footer text this outcome owes the reader, if any.
    pub(crate) fn footer(self) -> Option<String> {
        match self {
            GroundingOutcome::Flagged(block) => Some(block),
            // Advisory checks that started and could not finish say NOTHING to
            // the reader. This is safe only because nothing announces a check
            // any more (the "Checking against source…" status line is gone):
            // silence is uninformative everywhere, never an implied
            // verification. The failure is a log line for operators
            // (`claim grounding timed out` / `failed`), not a caveat the reader
            // cannot act on.
            GroundingOutcome::Incomplete
            | GroundingOutcome::PartiallyChecked
            | GroundingOutcome::NothingChecked
            | GroundingOutcome::Unsettled => None,
            GroundingOutcome::NotRun | GroundingOutcome::Clean | GroundingOutcome::Cancelled => {
                None
            }
        }
    }

    /// What a finished pass concluded. A finding stands on its own: the reader
    /// is told what is wrong, not how much was covered.
    pub(crate) fn of_report(report: &crate::claim_verifier::GroundingReport) -> Self {
        match report.render() {
            Some(block) => GroundingOutcome::Flagged(block),
            None if report.claims.is_empty() => GroundingOutcome::NothingChecked,
            None if !report.complete => GroundingOutcome::PartiallyChecked,
            None if report.is_clean() => GroundingOutcome::Clean,
            None => GroundingOutcome::Unsettled,
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            GroundingOutcome::NotRun => "not_run",
            GroundingOutcome::Clean => "clean",
            GroundingOutcome::NothingChecked => "nothing_checked",
            GroundingOutcome::Unsettled => "unsettled",
            GroundingOutcome::PartiallyChecked => "partially_checked",
            GroundingOutcome::Flagged(_) => "flagged",
            GroundingOutcome::Incomplete => "incomplete",
            GroundingOutcome::Cancelled => "cancelled",
        }
    }
}

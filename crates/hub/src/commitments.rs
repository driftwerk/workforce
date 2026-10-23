//! Post-turn commitment enforcement (P1).
//!
//! A turn can end on "I'll post the report at 5pm" with nothing scheduled to
//! make that happen — the promise silently dies with the turn. These pure
//! helpers back the delivery-site check: a deterministic short-circuit (a
//! successful scheduling-class call this turn is real machinery), then one
//! cheap classifier call over the answer tail, and a visible caveat when an
//! unconditional commitment has no backing.

use serde::Deserialize;
use workforce_agent::runner::ToolInvocation;
use workforce_common::text::truncate_with_ellipsis;

use crate::turn_extraction::strip_code_fence;

/// Tools whose successful invocation constitutes real machinery behind a
/// future commitment. Exact names plus the heartbeat.* family.
const BACKING_TOOLS: &[&str] = &["scheduler", "reminder", "jobs", "watch", "monitor"];

/// True when any scheduling-class tool call succeeded this turn — the
/// deterministic short-circuit that skips the classifier entirely.
pub fn has_backing_call(invocations: &[ToolInvocation]) -> bool {
    invocations.iter().any(|inv| {
        inv.success
            && (BACKING_TOOLS.contains(&inv.tool_name.as_str())
                || inv.tool_name.starts_with("heartbeat."))
    })
}

/// Chars of answer tail given to the classifier (commitments cluster at the end).
pub const CLASSIFIER_TAIL_CHARS: usize = 6_000;
/// Cap on the echoed commitment text in the caveat.
const COMMITMENT_MAX_CHARS: usize = 200;

/// The char-safe last [`CLASSIFIER_TAIL_CHARS`] characters of the answer.
pub fn answer_tail(answer: &str) -> &str {
    let n_chars = answer.chars().count();
    if n_chars <= CLASSIFIER_TAIL_CHARS {
        return answer;
    }
    let skip = n_chars - CLASSIFIER_TAIL_CHARS;
    match answer.char_indices().nth(skip) {
        Some((idx, _)) => &answer[idx..],
        None => answer,
    }
}

pub const COMMITMENT_SYSTEM: &str = "You check an assistant's final message for an \
    UNCONDITIONAL commitment by the assistant to take a specific future action itself \
    (e.g. \"I'll post the report at 5pm\", \"I will follow up tomorrow\"). NOT commitments: \
    conditional offers (\"I can do X if you'd like\"), requests that the USER act, \
    descriptions of already-completed work, scheduled jobs the message says were just \
    created, or hypotheticals. Reply with ONLY JSON: \
    {\"commitment\": \"<the quoted commitment, short>\"} if one exists, else \
    {\"commitment\": null}. No prose, no fences.";

#[derive(Deserialize)]
struct RawCommitment {
    commitment: Option<String>,
}

/// Parse the classifier's reply into the (bounded) commitment text. `None` on
/// any parse failure, a `null`, or an empty string — every failure mode reads
/// as "no commitment", never a spurious caveat.
pub fn parse_commitment(text: &str) -> Option<String> {
    let cleaned = strip_code_fence(text.trim());
    let raw: RawCommitment = serde_json::from_str(cleaned).ok()?;
    let commitment = raw.commitment?;
    let trimmed = commitment.trim();
    (!trimmed.is_empty()).then(|| truncate_with_ellipsis(trimmed, COMMITMENT_MAX_CHARS))
}

/// The visible caveat appended when a commitment has no backing machinery.
pub fn caveat_block(commitment: &str) -> String {
    format!(
        "⚠ *Unbacked commitment:* I said I'd {commitment} but nothing is scheduled to \
         make that happen. Say 'do it now' or ask me to schedule it — otherwise it \
         likely won't happen."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation(tool: &str, success: bool) -> ToolInvocation {
        ToolInvocation {
            tool_name: tool.to_string(),
            input: serde_json::Value::Null,
            output: serde_json::Value::Null,
            success,
            policy_decision: "allow".to_string(),
            duration_ms: 1,
            denial_reason: None,
            error: (!success).then(|| "boom".to_string()),
        }
    }

    #[test]
    fn backing_call_requires_success_on_a_scheduling_class_tool() {
        assert!(has_backing_call(&[invocation("scheduler", true)]));
        assert!(has_backing_call(&[invocation("reminder", true)]));
        assert!(has_backing_call(&[invocation("jobs", true)]));
        assert!(has_backing_call(&[invocation("watch", true)]));
        assert!(has_backing_call(&[invocation("monitor", true)]));
        assert!(has_backing_call(&[invocation("heartbeat.create", true)]));
        assert!(has_backing_call(&[invocation("heartbeat.update", true)]));

        assert!(
            !has_backing_call(&[invocation("scheduler", false)]),
            "a FAILED scheduler call is not backing"
        );
        assert!(!has_backing_call(&[invocation("jira.search", true)]));
        assert!(
            !has_backing_call(&[invocation("heartbeat", true)]),
            "bare `heartbeat` is not in the family"
        );
        assert!(!has_backing_call(&[]));
        assert!(has_backing_call(&[
            invocation("jira.search", true),
            invocation("scheduler", false),
            invocation("scheduler", true),
        ]));
    }

    #[test]
    fn answer_tail_is_char_safe_and_keeps_the_end() {
        let short = "short answer";
        assert_eq!(answer_tail(short), short);

        let long = format!("{}é-tail-marker", "α".repeat(CLASSIFIER_TAIL_CHARS));
        let tail = answer_tail(&long);
        assert_eq!(tail.chars().count(), CLASSIFIER_TAIL_CHARS);
        assert!(tail.ends_with("é-tail-marker"));
        assert!(tail.starts_with('α'), "cut lands on a char boundary");
    }

    #[test]
    fn parse_commitment_reads_json_and_tolerates_fences() {
        assert_eq!(
            parse_commitment("{\"commitment\": \"post the report at 5pm\"}").as_deref(),
            Some("post the report at 5pm")
        );
        assert_eq!(
            parse_commitment("```json\n{\"commitment\": \"follow up tomorrow\"}\n```").as_deref(),
            Some("follow up tomorrow")
        );
    }

    #[test]
    fn parse_commitment_null_empty_or_garbage_is_none() {
        assert_eq!(parse_commitment("{\"commitment\": null}"), None);
        assert_eq!(parse_commitment("{\"commitment\": \"\"}"), None);
        assert_eq!(parse_commitment("{\"commitment\": \"   \"}"), None);
        assert_eq!(parse_commitment("{}"), None);
        assert_eq!(parse_commitment("no commitment found"), None);
        assert_eq!(parse_commitment(""), None);
    }

    #[test]
    fn parse_commitment_truncates_a_rambling_echo() {
        let long = format!("{{\"commitment\": \"{}\"}}", "x".repeat(500));
        let parsed = parse_commitment(&long).unwrap();
        assert!(parsed.ends_with('…'));
        assert!(parsed.len() <= COMMITMENT_MAX_CHARS + '…'.len_utf8());
    }

    #[test]
    fn caveat_block_format_is_stable() {
        assert_eq!(
            caveat_block("post the report at 5pm"),
            "⚠ *Unbacked commitment:* I said I'd post the report at 5pm but nothing is \
             scheduled to make that happen. Say 'do it now' or ask me to schedule it — \
             otherwise it likely won't happen."
        );
    }
}

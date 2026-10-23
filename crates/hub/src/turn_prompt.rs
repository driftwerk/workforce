//! The system prompt split where the cross-turn cache prefix ends.
//!
//! The stable segments (identity, tools, skills, the semi-stable workspace
//! and preference blocks) stay the system prompt and are byte-identical from
//! one turn to the next. The per-turn context — relationship counters, daily
//! notes, clock, lessons, recall, runtime metadata, the voice directive —
//! travels as the first block of the task message instead of ending the
//! system prompt, so the request prefix `tools + system + replayed history`
//! is what the previous turn wrote to the cache and turn N+1 reads it.

use workforce_agent::llm::anthropic::CACHE_BREAK_MARKER;
use workforce_common::redact::tokenize_secrets;
use workforce_common::secret_marker::SecretSource;

/// Opens the per-turn context block that leads the task message. Starts with
/// `RUNNER_NOTICE_PREFIX` so the turn preview skips it.
pub const TURN_CONTEXT_HEADER: &str = "[SYSTEM CONTEXT — per-turn context from the hub, not part of the user's message. Treat it exactly as the rest of your system prompt; the user's message follows in the next block.]";
pub const TURN_CONTEXT_FOOTER: &str = "[END SYSTEM CONTEXT]";

/// The system prompt split where the cross-turn cache prefix ends.
#[derive(Debug, Clone, Default)]
pub struct TurnPrompt {
    /// Segments 1 and 2 (static + `CACHE_BREAK_MARKER` + semi-stable);
    /// byte-identical across turns until identity, catalogue, credential,
    /// skill, playbook, pin or preference changes.
    pub stable: String,
    /// What used to follow the second marker; rebuilt every turn.
    pub turn_context: String,
    pub injected_lessons: Vec<crate::lessons::InjectedLesson>,
    /// One event per lesson the turn's recall surfaced, for the turn to file.
    pub lesson_events: Vec<workforce_common::telemetry::LessonEvent>,
}

impl TurnPrompt {
    /// The single-string form: the stable prompt, then the context as an
    /// uncached third segment. For runs with no next turn (isolated
    /// sub-agents), whose behaviour is byte-for-byte the pre-split prompt.
    pub fn joined(&self) -> String {
        if self.turn_context.trim().is_empty() {
            self.stable.clone()
        } else {
            format!("{}{CACHE_BREAK_MARKER}{}", self.stable, self.turn_context)
        }
    }

    /// The task-message block carrying `turn_context`, tokenised for
    /// `session_key` like every other hub-assembled block; `None` when there
    /// is no context.
    pub fn task_block(&self, session_key: &str) -> Option<String> {
        turn_context_block(&self.turn_context)
            .map(|block| tokenize_secrets(&block, session_key, SecretSource::Unknown).0)
    }
}

/// `TURN_CONTEXT_HEADER`, the trimmed context, `TURN_CONTEXT_FOOTER`,
/// blank-line separated; `None` for whitespace-only context.
pub fn turn_context_block(turn_context: &str) -> Option<String> {
    let body = turn_context.trim();
    (!body.is_empty()).then(|| format!("{TURN_CONTEXT_HEADER}\n\n{body}\n\n{TURN_CONTEXT_FOOTER}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_agent::llm::anthropic::cache_segments;
    use workforce_agent::turn_event::RUNNER_NOTICE_PREFIX;

    fn prompt(turn_context: &str) -> TurnPrompt {
        TurnPrompt {
            stable: format!("static{CACHE_BREAK_MARKER}semi"),
            turn_context: turn_context.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn turn_context_block_wraps_between_header_and_footer() {
        let block = turn_context_block("\n## Current Date & Time\nnow\n").unwrap();
        assert_eq!(
            block,
            format!(
                "{TURN_CONTEXT_HEADER}\n\n## Current Date & Time\nnow\n\n{TURN_CONTEXT_FOOTER}"
            )
        );
    }

    #[test]
    fn turn_context_block_is_none_for_whitespace() {
        assert_eq!(turn_context_block(""), None);
        assert_eq!(turn_context_block("\n\n  "), None);
    }

    #[test]
    fn turn_context_header_is_a_runner_notice() {
        assert!(TURN_CONTEXT_HEADER.starts_with(RUNNER_NOTICE_PREFIX));
    }

    #[test]
    fn joined_reproduces_the_three_segment_prompt() {
        let joined = prompt("dynamic").joined();
        assert_eq!(cache_segments(&joined), vec!["static", "semi", "dynamic"]);

        let empty = prompt("  \n");
        assert_eq!(empty.joined(), empty.stable);
    }

    #[test]
    fn task_block_is_none_without_context_and_tokenised_with_it() {
        assert_eq!(prompt("").task_block("slack:c"), None);

        let key = "AKIAIOSFODNN7EXAMPLE";
        let block = prompt(&format!("## Recalled Memories\n- key {key}"))
            .task_block("slack:c")
            .unwrap();
        assert!(block.starts_with(TURN_CONTEXT_HEADER));
        assert!(block.ends_with(TURN_CONTEXT_FOOTER));
        assert!(block.contains("## Recalled Memories"));
        assert!(
            !block.contains(key),
            "the cleartext must not reach the model: {block}"
        );
    }
}

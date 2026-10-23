//! Shared building blocks for per-turn memory extractors.
//!
//! The passive-memory and lessons-learned pipelines both: (1) gate background-
//! agent channels and recap-shaped user messages, (2) salvage JSON from messy
//! LLM output, and (3) insert-or-merge against near-duplicates within a
//! time window. Each of those concerns lives here once.

use std::sync::Arc;

use eyre::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{Memory, MemoryFilter, MemoryLayer, MemorySource};

// ─── Channel gate ────────────────────────────────────────────────────────────

const CONSOLIDATION_CHANNEL: &str = "memory-consolidation";
const KNOWLEDGE_MAINTENANCE_CHANNEL: &str = "knowledge-maintenance";
const SELF_LEARNING_CHANNEL: &str = "self-learning";
const SUBAGENT_CHANNEL: &str = "subagent";
const SCHEDULER_PREFIX: &str = "scheduler:";
const WORKFLOW_PREFIX: &str = "workflow:";

/// The channel prefix of an eval turn: answered as its person's own turn,
/// and left out of everything a later turn could learn from.
pub const EVAL_CHANNEL_PREFIX: &str = "eval:";

/// Returns `true` when the channel identifies a non-conversational background
/// agent that should not produce episodic memories.
pub fn is_internal_channel(channel: &str) -> bool {
    matches!(
        channel,
        CONSOLIDATION_CHANNEL
            | KNOWLEDGE_MAINTENANCE_CHANNEL
            | SELF_LEARNING_CHANNEL
            | SUBAGENT_CHANNEL
    ) || channel.starts_with(SCHEDULER_PREFIX)
        || channel.starts_with(WORKFLOW_PREFIX)
}

/// Returns `true` for a turn asked by `workforce eval`.
pub fn is_eval_channel(channel: &str) -> bool {
    channel.starts_with(EVAL_CHANNEL_PREFIX)
}

/// Returns `true` when nothing of the channel's turns may reach a later
/// turn: a background agent's, or an eval's.
pub fn leaves_no_memory(channel: &str) -> bool {
    is_internal_channel(channel) || is_eval_channel(channel)
}

// ─── Recap detection ────────────────────────────────────────────────────────

/// We match on the *user's* message — recap requests are structurally distinct
/// imperatives, while recap responses read indistinguishably from primary
/// narrative prose.
static RECAP_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"(?i)\b(summari[sz]e|summary|summing up)\b[^.?!\n]{0,80}\b(we|our|us|today|yesterday|tomorrow|past|last|recent|history|conversation|chat|talked|discussed|went over|covered)\b",
        r"(?i)\b(recap|catch me up|bring me up to speed|fill me in)\b",
        r"(?i)\bremind me (what|about|of|when|how|that)\b",
        r"(?i)\bwhat (did|have|were|are|was) (we|us|our)\b",
        r"(?i)\bwhat( has|'s| have)? happened\b[^.?!\n]{0,30}\b(today|yesterday|earlier|last|this (morning|afternoon|evening|week|month)|recently|so far)\b",
        r"(?i)\b(do|did) you remember\b[^.?!\n]{0,30}\b(what|when|that|about|how|where)\b",
        r"(?i)\b(tell me|go over) (what|how) (we|us|our|i)\b",
        r"(?i)\bwhat( has|'s| have)? been going on\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("static recap pattern compiles"))
    .collect()
});

/// Returns `true` when the user's message looks like a request to recap or
/// recall prior conversation rather than a description of fresh events.
pub fn looks_like_recap_request(user_message: &str) -> bool {
    let trimmed = user_message.trim();
    if trimmed.is_empty() {
        return false;
    }
    let head: String = trimmed.chars().take(800).collect();
    RECAP_PATTERNS.iter().any(|re| re.is_match(&head))
}

/// Outcome of the pre-extraction gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractionGate {
    Proceed,
    SkipInternalChannel,
    SkipEvalChannel,
    SkipRecapRequest,
}

impl ExtractionGate {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Proceed => "proceed",
            Self::SkipInternalChannel => "internal_channel",
            Self::SkipEvalChannel => "eval_channel",
            Self::SkipRecapRequest => "recap_request",
        }
    }
}

/// Combine channel and recap gates. Channel takes precedence because
/// background agents don't have "users" in the conversational sense.
pub fn gate(channel: &str, user_message: &str) -> ExtractionGate {
    if is_internal_channel(channel) {
        return ExtractionGate::SkipInternalChannel;
    }
    if is_eval_channel(channel) {
        return ExtractionGate::SkipEvalChannel;
    }
    if looks_like_recap_request(user_message) {
        return ExtractionGate::SkipRecapRequest;
    }
    ExtractionGate::Proceed
}

// ─── JSON salvage ───────────────────────────────────────────────────────────

pub fn strip_code_fence(s: &str) -> &str {
    s.trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
}

/// Span from the first `[` to the last `]`. Naïve about `]` inside string
/// literals; callers re-validate with `serde_json` so a wrong slice fails
/// the parse rather than returning bad data.
pub fn first_bracketed_array(s: &str) -> Option<&str> {
    let start = s.find('[')?;
    let end = s.rfind(']')?;
    (end > start).then_some(&s[start..=end])
}

/// Complete top-level `{...}` spans inside the first `[` of `s`, for output
/// truncated mid-array (an LLM hitting its token cap drops the closing `]`,
/// which defeats [`first_bracketed_array`]). String- and escape-aware, so
/// braces inside JSON string literals don't skew the depth count. Callers
/// parse each span individually — one malformed object doesn't discard the
/// rest.
pub fn complete_objects_of_truncated_array(s: &str) -> Vec<&str> {
    let Some(start) = s.find('[') else {
        return Vec::new();
    };
    let mut objects = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut object_start = None;
    for (i, b) in s.bytes().enumerate().skip(start + 1) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    object_start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some(from) = object_start.take() {
                        objects.push(&s[from..=i]);
                    }
                }
            }
            b']' if depth == 0 => break,
            _ => {}
        }
    }
    objects
}

// ─── Dedup ──────────────────────────────────────────────────────────────────

/// What [`remember_with_dedup`] did with a candidate memory.
#[derive(Debug)]
pub enum WriteOutcome {
    /// Brand-new memory inserted.
    Inserted,
    /// A near-duplicate was found and the recurrence counter was incremented.
    MergedInto {
        /// ID of the existing memory the candidate merged into. Lets callers
        /// apply secondary effects (e.g. lesson recurrence-importance bump)
        /// to the right row without an extra recall.
        id: workforce_memory::types::MemoryId,
        recurrence_count: u32,
    },
}

/// Dedup window for passive narrative memories. 6h is long enough to catch a
/// back-to-back retry storm but short enough that the same topic surfacing a
/// week later counts as a new event, not a recurrence.
pub const PASSIVE_DEDUP_WINDOW: chrono::Duration = chrono::Duration::hours(6);

/// Dedup window for lessons. Long because lessons are durable — the agent
/// learning the same gotcha 30 days apart is a recurrence ("this lesson
/// isn't sticking"), not two independent observations.
pub const LESSON_DEDUP_WINDOW: chrono::Duration = chrono::Duration::days(30);

/// Near-duplicate certainty bar for lessons. Deliberately well below the
/// generic [`DEFAULT_NEAR_DUPLICATE_SIMILARITY`] (0.92): with the production
/// embedding model an *identical* lesson re-embeds at only ~0.87 certainty, a
/// genuine paraphrase of the same lesson lands ~0.74, and an unrelated lesson
/// ~0.64. At 0.92 dedup could never fire, so re-extractions of the same gotcha
/// accumulated as duplicates. 0.80 merges true re-extractions of substantially
/// the same lesson while leaving complementary facets (~0.74) distinct and
/// staying clear of the ~0.64 unrelated floor.
pub const LESSON_NEAR_DUPLICATE_SIMILARITY: f32 = 0.80;

/// Per-extractor dedup parameters.
#[derive(Debug, Clone)]
pub struct DedupConfig {
    /// Restrict the dedup search to a single memory layer.
    pub layer: MemoryLayer,
    /// When set, restrict the dedup search to one [`MemorySource`] — lessons
    /// look only at other lessons, not at consolidated semantic memories.
    pub source: Option<MemorySource>,
    /// Anything older than `now - window` is treated as a separate event,
    /// not a recurrence of the same one.
    pub window: chrono::Duration,
    /// Minimum vector certainty for a candidate to count as a near-duplicate
    /// of an existing memory. Per-config because the right value depends on
    /// content shape: see [`LESSON_NEAR_DUPLICATE_SIMILARITY`] for why lessons
    /// need a lower bar than the generic [`DEFAULT_NEAR_DUPLICATE_SIMILARITY`].
    pub near_duplicate_similarity: f32,
}

/// Try to record `candidate` as a recurrence of an existing near-duplicate;
/// fall back to a fresh insert if none qualifies.
///
/// Scoping rules — both must hold for a candidate to merge:
/// * same `owner` agent and same `identity_id` (no cross-user collapse),
/// * either same `source_session_id` OR existing memory created within
///   `config.window` (no cross-time collapse of legitimate recurrences).
pub async fn remember_with_dedup(
    store: &dyn MemoryStore,
    candidate: Memory,
    config: DedupConfig,
) -> Result<WriteOutcome> {
    let cutoff = chrono::Utc::now() - config.window;
    let dedup_filter = MemoryFilter {
        owner: Some(candidate.owner.clone()),
        layer: Some(config.layer.clone()),
        source: config.source,
        identity_id: candidate.identity_id.clone(),
        created_after: Some(cutoff),
        ..Default::default()
    };

    let near = store
        .find_near_duplicate(
            &candidate.content,
            dedup_filter,
            config.near_duplicate_similarity,
        )
        .await
        .ok()
        .flatten();

    if let Some(existing) = near {
        let same_identity = existing.memory.identity_id == candidate.identity_id;
        let same_session = matches!(
            (
                existing.memory.source_session_id.as_ref(),
                candidate.source_session_id.as_ref(),
            ),
            (Some(a), Some(b)) if a == b
        );
        let within_window = existing.memory.created_at >= cutoff;

        if same_identity && (same_session || within_window) {
            let merged_id = existing.memory.id.clone();
            let count = store
                .record_recurrence(&merged_id, candidate.source_message_ids)
                .await?;
            return Ok(WriteOutcome::MergedInto {
                id: merged_id,
                recurrence_count: count,
            });
        }
    }

    store.remember(candidate).await?;
    Ok(WriteOutcome::Inserted)
}

/// Convenience type alias used by call sites that pass the store around as
/// an `Arc<dyn MemoryStore>` — avoids leaking the trait-object dance.
pub type SharedMemoryStore = Arc<dyn MemoryStore>;

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::sync::Arc as StdArc;
    use workforce_core::types::AgentId;
    use workforce_memory::store::{
        MemoryStore, MemoryStoreCacheStats, DEFAULT_NEAR_DUPLICATE_SIMILARITY,
    };
    use workforce_memory::types::{
        Memory, MemoryFilter, MemoryId, MemorySearchResult, MemorySource, Visibility,
    };

    // ─── Channel gate ───────────────────────────────────────────────────

    #[test]
    fn internal_channel_blocks_consolidation() {
        assert!(is_internal_channel("memory-consolidation"));
    }

    #[test]
    fn internal_channel_blocks_scheduler_subchannels() {
        assert!(is_internal_channel("scheduler:daily-briefing"));
        assert!(is_internal_channel("scheduler:foo"));
        assert!(is_internal_channel("scheduler:"));
    }

    #[test]
    fn internal_channel_blocks_workflow_subchannels() {
        assert!(is_internal_channel("workflow:abc-123"));
        assert!(is_internal_channel("workflow:"));
    }

    #[test]
    fn internal_channel_blocks_subagent_and_knowledge_maintenance() {
        assert!(is_internal_channel("subagent"));
        assert!(is_internal_channel("knowledge-maintenance"));
    }

    #[test]
    fn internal_channel_blocks_self_learning() {
        // Curation/deep-expertise turns are background chatter; extracting
        // them burned an LLM call per turn before dying at the identity check.
        assert!(is_internal_channel("self-learning"));
        assert_eq!(
            gate("self-learning", "deepen the scheduler node"),
            ExtractionGate::SkipInternalChannel
        );
    }

    #[test]
    fn internal_channel_allows_user_channels() {
        assert!(!is_internal_channel("slack:C123"));
        assert!(!is_internal_channel(
            "slack:allan:D0FAKEDMAA1:1778749293.028539"
        ));
        assert!(!is_internal_channel("dm:user-abc"));
        assert!(!is_internal_channel("ios"));
        assert!(!is_internal_channel("web"));
        assert!(!is_internal_channel("rest"));
        assert!(!is_internal_channel("terminal"));
        assert!(!is_internal_channel("chat"));
        assert!(!is_internal_channel(""));
    }

    #[test]
    fn internal_channel_rejects_prefix_lookalikes() {
        assert!(!is_internal_channel("memory-consolidation-results"));
        assert!(!is_internal_channel("not-scheduler:foo"));
        assert!(!is_internal_channel("not-workflow:foo"));
        assert!(!is_internal_channel("subagent-foo"));
        assert!(!is_internal_channel("knowledge-maintenance-results"));
    }

    // ─── Recap heuristic ────────────────────────────────────────────────

    fn assert_recap(msg: &str) {
        assert!(
            looks_like_recap_request(msg),
            "expected recap match for: {msg:?}"
        );
    }

    fn assert_not_recap(msg: &str) {
        assert!(
            !looks_like_recap_request(msg),
            "expected NO recap match for: {msg:?}"
        );
    }

    #[test]
    fn recap_matches_summary_of_shared_history() {
        assert_recap("hey can you briefly summarize what we talked about and what we did together in the last 2 weeks?");
        assert_recap("summarise our conversation today");
        assert_recap("can you give me a summary of what we discussed yesterday");
        assert_recap("Summary of our chat please");
    }

    #[test]
    fn recap_matches_direct_recap_verbs() {
        assert_recap("recap please");
        assert_recap("can you recap?");
        assert_recap("catch me up on where we are");
        assert_recap("bring me up to speed on the project");
        assert_recap("fill me in on what's been happening");
    }

    #[test]
    fn recap_matches_remind_me() {
        assert_recap("remind me what we decided last week");
        assert_recap("remind me about the Foxtons situation");
        assert_recap("remind me of that conversation");
        assert_recap("remind me when we agreed on the deadline");
    }

    #[test]
    fn recap_matches_what_did_we() {
        assert_recap("what did we do today?");
        assert_recap("what have we been working on?");
        assert_recap("what were we talking about?");
        assert_recap("what was our last decision?");
    }

    #[test]
    fn recap_matches_what_happened_with_time_scope() {
        assert_recap("what happened today");
        assert_recap("what's happened this week");
        assert_recap("what has happened recently");
        assert_recap("what happened earlier");
        assert_recap("what happened this morning");
        assert_recap("hey, what happened this morning?");
        assert_recap("what's happened this afternoon");
        assert_recap("what happened so far");
    }

    #[test]
    fn recap_matches_remember_probes() {
        assert_recap("do you remember what I said about Fabrikam?");
        assert_recap("did you remember when we last talked about it?");
    }

    #[test]
    fn recap_does_not_match_describing_past_events() {
        assert_not_recap("yesterday I drove to see Anne and we had lunch");
        assert_not_recap("earlier today I figured out the bug");
        assert_not_recap("last week was hectic, I shipped three features");
    }

    #[test]
    fn recap_does_not_match_short_summarise_of_content() {
        assert_not_recap("summarise this paragraph for me: lorem ipsum dolor sit amet");
        assert_not_recap("can you summarize the attached document");
        assert_not_recap("summary of this PR description");
    }

    #[test]
    fn recap_does_not_match_questions_about_future_or_state() {
        assert_not_recap("what's next on the roadmap?");
        assert_not_recap("how does the auth flow work?");
        assert_not_recap("can you add a test for this?");
        assert_not_recap("please run the build");
    }

    #[test]
    fn eval_turns_are_answered_but_never_learned_from() {
        assert_eq!(
            gate("eval:run-1:q1", "I just shipped the auth feature"),
            ExtractionGate::SkipEvalChannel
        );
        assert!(leaves_no_memory("eval:run-1:q1"));
        assert!(!is_internal_channel("eval:run-1:q1"));
        assert!(!leaves_no_memory("rest"));
    }

    #[test]
    fn recap_handles_empty_and_whitespace() {
        assert_not_recap("");
        assert_not_recap("   \n\t");
    }

    #[test]
    fn recap_caps_inspection_length() {
        let mut msg = "x".repeat(2000);
        msg.push_str(" recap please");
        assert_not_recap(&msg);
    }

    // ─── Gate composition ──────────────────────────────────────────────

    #[test]
    fn gate_skips_internal_channel_even_for_primary_turn() {
        assert_eq!(
            gate("memory-consolidation", "I just shipped the auth feature"),
            ExtractionGate::SkipInternalChannel
        );
        assert_eq!(
            gate("scheduler:nightly", "Anything to report?"),
            ExtractionGate::SkipInternalChannel
        );
    }

    #[test]
    fn gate_skips_recap_on_user_channel() {
        assert_eq!(
            gate("slack:C123", "summarise what we talked about today"),
            ExtractionGate::SkipRecapRequest
        );
    }

    #[test]
    fn gate_proceeds_for_primary_turn_on_user_channel() {
        assert_eq!(
            gate(
                "slack:C123",
                "I just had a long chat with Anne and feel uplifted"
            ),
            ExtractionGate::Proceed
        );
    }

    #[test]
    fn gate_channel_takes_precedence_over_recap() {
        assert_eq!(
            gate("memory-consolidation", "summarise what we discussed today"),
            ExtractionGate::SkipInternalChannel
        );
    }

    #[test]
    fn gate_treats_empty_channel_as_user_channel() {
        assert_eq!(
            gate("", "I had pizza with my kids"),
            ExtractionGate::Proceed
        );
        assert_eq!(
            gate("", "summarise our chat last week"),
            ExtractionGate::SkipRecapRequest
        );
    }

    // ─── JSON salvage ──────────────────────────────────────────────────

    #[test]
    fn strip_code_fence_handles_json_fence() {
        assert_eq!(strip_code_fence("```json\n[1,2]\n```"), "[1,2]");
        assert_eq!(strip_code_fence("```\n[1,2]\n```"), "[1,2]");
        assert_eq!(strip_code_fence("[1,2]"), "[1,2]");
    }

    #[test]
    fn first_bracketed_array_returns_span() {
        assert_eq!(
            first_bracketed_array("Here are the events: [{\"a\":1}, {\"b\":2}] OK"),
            Some("[{\"a\":1}, {\"b\":2}]")
        );
        assert_eq!(first_bracketed_array("no brackets here"), None);
        assert_eq!(first_bracketed_array("]first"), None);
        assert_eq!(first_bracketed_array("[only-open"), None);
    }

    #[test]
    fn complete_objects_salvages_truncated_array() {
        let truncated = r#"[
  {"content": "first event", "tags": ["a"]},
  {"content": "second event"},
  {"content": "third eve"#;
        let objects = complete_objects_of_truncated_array(truncated);
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0], r#"{"content": "first event", "tags": ["a"]}"#);
        assert_eq!(objects[1], r#"{"content": "second event"}"#);
    }

    #[test]
    fn complete_objects_ignores_braces_inside_strings() {
        let tricky = r#"[{"content": "has } brace and \" quote"}, {"content": "cut of"#;
        let objects = complete_objects_of_truncated_array(tricky);
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0], r#"{"content": "has } brace and \" quote"}"#);
    }

    #[test]
    fn complete_objects_handles_nested_and_absent_arrays() {
        let nested = r#"[{"content": "x", "meta": {"k": 1}}, {"content": "y"#;
        let objects = complete_objects_of_truncated_array(nested);
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0], r#"{"content": "x", "meta": {"k": 1}}"#);

        assert!(complete_objects_of_truncated_array("no array at all").is_empty());
        // A complete array stops the scan at its closing bracket.
        let complete = r#"[{"content": "a"}] trailing {"content": "not in array"}"#;
        assert_eq!(complete_objects_of_truncated_array(complete).len(), 1);
    }

    // ─── remember_with_dedup ───────────────────────────────────────────

    struct DedupMockStore {
        seeded: Mutex<Option<MemorySearchResult>>,
        remembered: Mutex<Vec<Memory>>,
        recurrences: Mutex<Vec<(MemoryId, Vec<String>)>>,
        record_recurrence_count: Mutex<u32>,
    }

    impl DedupMockStore {
        fn new() -> Self {
            Self {
                seeded: Mutex::new(None),
                remembered: Mutex::new(Vec::new()),
                recurrences: Mutex::new(Vec::new()),
                record_recurrence_count: Mutex::new(0),
            }
        }
        fn seed_existing(&self, mem: Memory, score: f32) {
            *self.seeded.lock() = Some(MemorySearchResult { memory: mem, score });
        }
        fn remembered_count(&self) -> usize {
            self.remembered.lock().len()
        }
        fn recurrences_count(&self) -> usize {
            self.recurrences.lock().len()
        }
        fn last_recurrence_message_ids(&self) -> Option<Vec<String>> {
            self.recurrences.lock().last().map(|(_, ids)| ids.clone())
        }
    }

    #[async_trait]
    impl MemoryStore for DedupMockStore {
        fn cache_stats(&self) -> MemoryStoreCacheStats {
            MemoryStoreCacheStats::default()
        }
        async fn remember(&self, memory: Memory) -> Result<MemoryId> {
            let id = memory.id.clone();
            self.remembered.lock().push(memory);
            Ok(id)
        }
        async fn recall(
            &self,
            _query: &str,
            _filter: MemoryFilter,
        ) -> Result<Vec<MemorySearchResult>> {
            Ok(self.seeded.lock().clone().into_iter().collect())
        }
        async fn get(&self, _id: &MemoryId) -> Result<Option<Memory>> {
            Ok(None)
        }
        async fn forget(&self, _id: &MemoryId) -> Result<()> {
            Ok(())
        }
        async fn share(&self, _id: &MemoryId, _v: Visibility) -> Result<()> {
            Ok(())
        }
        async fn count(&self, _filter: MemoryFilter) -> Result<usize> {
            Ok(0)
        }
        async fn list(&self, _filter: MemoryFilter) -> Result<Vec<Memory>> {
            Ok(Vec::new())
        }
        async fn update_tags(&self, _id: &MemoryId, _tags: Vec<String>) -> Result<()> {
            Ok(())
        }
        async fn adjust_importance(&self, _id: &MemoryId, _delta: f32) -> Result<f32> {
            Ok(0.5)
        }
        async fn identity_memories(&self, _: &AgentId, _: &str) -> Result<Vec<MemoryId>> {
            Ok(Vec::new())
        }
        async fn identity_counts(
            &self,
            _: &AgentId,
        ) -> Result<std::collections::BTreeMap<String, usize>> {
            Ok(std::collections::BTreeMap::new())
        }
        async fn reassign_identity(&self, _: &AgentId, _: &str, _: &str) -> Result<usize> {
            Ok(0)
        }
        async fn purge_identity(&self, _: &AgentId, _: &str) -> Result<usize> {
            Ok(0)
        }
        async fn purge_memories(&self, _: &[MemoryId]) -> Result<usize> {
            Ok(0)
        }
        async fn record_recurrence(
            &self,
            id: &MemoryId,
            additional_message_ids: Vec<String>,
        ) -> Result<u32> {
            self.recurrences
                .lock()
                .push((id.clone(), additional_message_ids));
            let mut c = self.record_recurrence_count.lock();
            *c = c.saturating_add(1);
            Ok(*c)
        }
    }

    fn make_candidate(content: &str, identity: Option<&str>, session: Option<&str>) -> Memory {
        let mut mem = Memory::new(content, MemoryLayer::Episodic, AgentId::new("agent"))
            .with_source(MemorySource::Passive);
        if let Some(uid) = identity {
            mem = mem.with_identity_id(uid);
        }
        if let Some(sid) = session {
            mem = mem.with_source_session(sid, vec!["new-msg".to_string()]);
        }
        mem
    }

    fn passive_config() -> DedupConfig {
        DedupConfig {
            layer: MemoryLayer::Episodic,
            source: None,
            window: PASSIVE_DEDUP_WINDOW,
            near_duplicate_similarity: DEFAULT_NEAR_DUPLICATE_SIMILARITY,
        }
    }

    #[tokio::test]
    async fn dedup_merges_when_similarity_above_threshold_and_same_identity() {
        let store = StdArc::new(DedupMockStore::new());
        let existing = make_candidate(
            "user asked me to run knowledge.index, I couldn't",
            Some("user-1"),
            Some("sess-A"),
        );
        store.seed_existing(existing, 0.95);

        let candidate = make_candidate(
            "the user asked me to run knowledge.index but I couldn't",
            Some("user-1"),
            Some("sess-A"),
        );
        let outcome = remember_with_dedup(store.as_ref(), candidate, passive_config())
            .await
            .unwrap();
        assert!(matches!(outcome, WriteOutcome::MergedInto { .. }));
        assert_eq!(store.remembered_count(), 0);
        assert_eq!(store.recurrences_count(), 1);
        assert_eq!(
            store.last_recurrence_message_ids().unwrap(),
            vec!["new-msg".to_string()]
        );
    }

    #[tokio::test]
    async fn dedup_refuses_to_merge_across_identities() {
        let store = StdArc::new(DedupMockStore::new());
        let existing = make_candidate("same topic", Some("user-1"), Some("sess-A"));
        store.seed_existing(existing, 0.99);

        let candidate = make_candidate("same topic", Some("user-2"), Some("sess-B"));
        let outcome = remember_with_dedup(store.as_ref(), candidate, passive_config())
            .await
            .unwrap();
        assert!(
            matches!(outcome, WriteOutcome::Inserted),
            "must not merge memories across identities"
        );
        assert_eq!(store.remembered_count(), 1);
        assert_eq!(store.recurrences_count(), 0);
    }

    #[tokio::test]
    async fn dedup_refuses_to_merge_old_existing_from_different_session() {
        let store = StdArc::new(DedupMockStore::new());
        let mut existing = make_candidate("same topic", Some("user-1"), Some("sess-OLD"));
        existing.created_at = chrono::Utc::now() - chrono::Duration::days(7);
        store.seed_existing(existing, 0.95);

        let candidate = make_candidate("same topic again", Some("user-1"), Some("sess-NEW"));
        let outcome = remember_with_dedup(store.as_ref(), candidate, passive_config())
            .await
            .unwrap();
        assert!(
            matches!(outcome, WriteOutcome::Inserted),
            "must not collapse a week-old memory from a different session"
        );
    }

    #[tokio::test]
    async fn dedup_merges_within_window_even_across_sessions() {
        let store = StdArc::new(DedupMockStore::new());
        let mut existing = make_candidate("same topic", Some("user-1"), Some("sess-A"));
        existing.created_at = chrono::Utc::now() - chrono::Duration::hours(1);
        store.seed_existing(existing, 0.95);

        let candidate = make_candidate("same topic", Some("user-1"), Some("sess-B"));
        let outcome = remember_with_dedup(store.as_ref(), candidate, passive_config())
            .await
            .unwrap();
        assert!(matches!(outcome, WriteOutcome::MergedInto { .. }));
    }

    #[tokio::test]
    async fn dedup_inserts_when_no_existing_match() {
        let store = StdArc::new(DedupMockStore::new());
        let candidate = make_candidate("first time we see this", Some("user-1"), Some("sess-A"));
        let outcome = remember_with_dedup(store.as_ref(), candidate, passive_config())
            .await
            .unwrap();
        assert!(matches!(outcome, WriteOutcome::Inserted));
        assert_eq!(store.remembered_count(), 1);
    }

    #[tokio::test]
    async fn dedup_lesson_window_merges_across_days() {
        let store = StdArc::new(DedupMockStore::new());
        let mut existing = make_candidate("lesson", Some("user-1"), Some("sess-A"));
        existing.created_at = chrono::Utc::now() - chrono::Duration::days(10);
        store.seed_existing(existing, 0.95);

        let candidate = make_candidate("lesson", Some("user-1"), Some("sess-B"));
        let lesson_cfg = DedupConfig {
            layer: MemoryLayer::Semantic,
            source: Some(MemorySource::Lesson),
            window: LESSON_DEDUP_WINDOW,
            near_duplicate_similarity: LESSON_NEAR_DUPLICATE_SIMILARITY,
        };
        let outcome = remember_with_dedup(store.as_ref(), candidate, lesson_cfg)
            .await
            .unwrap();
        assert!(
            matches!(outcome, WriteOutcome::MergedInto { .. }),
            "10 days is well within the 30-day lesson dedup window"
        );
    }

    #[tokio::test]
    async fn lesson_bar_merges_paraphrase_that_passive_bar_rejects() {
        // Calibration guard: a 0.85-certainty match is a re-extraction of the
        // same lesson and must merge under LESSON_NEAR_DUPLICATE_SIMILARITY
        // (0.80), but stays a separate insert under the generic 0.92 bar.
        let candidate_lesson = make_candidate("lesson", Some("user-1"), Some("sess-B"));
        let lesson_store = StdArc::new(DedupMockStore::new());
        lesson_store.seed_existing(
            make_candidate("lesson", Some("user-1"), Some("sess-A")),
            0.85,
        );
        let lesson_cfg = DedupConfig {
            layer: MemoryLayer::Semantic,
            source: Some(MemorySource::Lesson),
            window: LESSON_DEDUP_WINDOW,
            near_duplicate_similarity: LESSON_NEAR_DUPLICATE_SIMILARITY,
        };
        let merged = remember_with_dedup(lesson_store.as_ref(), candidate_lesson, lesson_cfg)
            .await
            .unwrap();
        assert!(
            matches!(merged, WriteOutcome::MergedInto { .. }),
            "0.85 ≥ 0.80 lesson bar → merge"
        );

        let candidate_passive = make_candidate("lesson", Some("user-1"), Some("sess-B"));
        let passive_store = StdArc::new(DedupMockStore::new());
        passive_store.seed_existing(
            make_candidate("lesson", Some("user-1"), Some("sess-A")),
            0.85,
        );
        let inserted =
            remember_with_dedup(passive_store.as_ref(), candidate_passive, passive_config())
                .await
                .unwrap();
        assert!(
            matches!(inserted, WriteOutcome::Inserted),
            "0.85 < 0.92 passive bar → insert"
        );
    }
}

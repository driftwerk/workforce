//! Passive post-turn memory extraction.
//!
//! After each substantive user↔agent exchange, a lightweight LLM call extracts
//! narrative event memories and stores them directly in the memory store.
//! Runs as a fire-and-forget background task.
//!
//! Each turn may produce zero or more memories — the extraction prompt asks
//! for a JSON array of events (what happened, how the user felt, what the
//! agent advised, outcome). [`parse_events`] salvages every recoverable JSON
//! shape; output with nothing JSON-shaped in it is dropped (storing raw prose
//! as memories polluted recall on prod).
//!
//! ## Gating
//!
//! Two structural filters run before the LLM call to prevent the most common
//! sources of memory pollution:
//!
//! 1. **Channel filter** — turns produced by background agents (the memory
//!    consolidation job, any scheduler-spawned task) are skipped. These agents
//!    don't have user-visible conversations; treating their tool-chatter as
//!    episodic memory creates a self-amplifying loop where each consolidation
//!    run produces meta-memories ("I performed memory consolidation…") that
//!    the next run picks up and re-consolidates.
//!
//! 2. **Recap-request filter** — when the user asks the agent to summarise,
//!    recap, or recall prior conversation, the assistant's response is a
//!    narrative recap of past events, not a description of what happened in
//!    this turn. Extracting from a recap re-mints old events as fresh memories
//!    dated today. We detect the *user's intent* (cheap regex on the user
//!    message) rather than trying to classify the assistant's prose, because
//!    the user's request is structurally distinguishable while a recap reads
//!    indistinguishably from a primary narrative.

use std::sync::Arc;

use eyre::Result;
use serde::Deserialize;
use tracing::{debug, info, warn};

use workforce_agent::compaction::extract_text_from_response;
use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::llm::types::LlmMessage;
use workforce_agent::runner::with_served_log;
use workforce_common::cost::{CostEntry, CostTracker, TokenCounts};
use workforce_common::subscription::Origin;
use workforce_core::types::AgentId;
use workforce_memory::store::{MemoryStore, DEFAULT_NEAR_DUPLICATE_SIMILARITY};
use workforce_memory::types::{Memory, MemoryLayer, MemorySource};

use crate::turn_extraction::{
    complete_objects_of_truncated_array, first_bracketed_array, gate, remember_with_dedup,
    strip_code_fence, DedupConfig, ExtractionGate, WriteOutcome, PASSIVE_DEDUP_WINDOW,
};

pub use crate::turn_extraction::{is_internal_channel, looks_like_recap_request};

const DEFAULT_PASSIVE_PROMPT: &str = "\
You are this agent's long-term memory. From the user↔assistant exchange below \
extract the memories worth keeping as a JSON array.

Each array element is a distinct event memory with this shape:

  {
    \"content\":   \"<first-person narrative from the agent's perspective, \
                    including what the user said or did, how they felt, any \
                    advice or observation you gave, and the user's reaction>\",
    \"facts\":     [\"<concrete fact strings — see the FACTS section below>\"],
    \"tags\":      [\"<short topic tags, lowercase, kebab-case>\"],
    \"importance\": <float 0.0-1.0>,
    \"emotion\":   \"<optional one-word emotion, e.g. regret, excited, anxious>\"
  }

Rules:
- Write `content` as a specific remembered event from THIS turn — what just \
  happened, what the user said or did, what advice you gave. Do NOT mint \
  memories from prior context being recapped.
- Keep the two people straight. You are the assistant; the other party is the \
  user. Refer to the user as \"the user\", or by a name the exchange shows is \
  theirs — never by your own name, and never attribute your suggestions to \
  them or theirs to you. A memory that swaps the two is worse than no memory: \
  it is read back later as fact and cannot be told apart from a true one.
- Anchor time to the conversation date given in the payload. Write ABSOLUTE \
  dates in `content` (\"on 2026-06-01\", \"the evening of 2026-05-31\") — never \
  bare relative terms (\"yesterday\", \"last night\", \"this morning\", \"on \
  Thursday\") — so the memory still reads correctly when it is recalled weeks \
  later.
- If the assistant response is summarising, recapping, or recalling prior \
  conversation — i.e. the events being described did NOT happen in this turn \
  — return `[]`. Those memories already exist; restating them as fresh events \
  dates them wrong.
- Emit multiple events when a single turn covers distinct topics.
- Emit `[]` when the turn is routine (greetings, trivial lookups, tool chatter).
- Do NOT include: raw code blocks, file paths, command output, internal tool \
  reasoning, task-local implementation details.
- Importance: 0.2 for casual preferences, 0.5 for typical personal context, \
  0.7+ for emotionally-loaded or high-consequence events.

FACTS (critical — narrative alone is lossy):
- For every event, list the concrete facts a future recall would need to \
  answer questions about it. Each fact is a short phrase pinning ONE entity \
  or commitment — not a paragraph. Examples of what belongs in `facts`:
    * People named in the turn: \"person:Anne (girlfriend)\", \"person:Danny (trainer)\".
    * Places: \"location:Kampala, Uganda\", \"location:Slough\".
    * Dates and durations: \"date:2026-06-15\", \"duration:long weekend\".
    * Bookings / IDs / references: \"hotel:Speke Resort Munyonyo\", \
      \"flight:BA065\", \"booking-ref:XYZ123\", \"ticket:FAB-352\".
    * Logistics: \"vaccination:DPT booster\", \"vaccination:typhoid\".
    * Numbers, prices, quantities: \"price:£1240\", \"distance:140mi round trip\".
- Prefer many short specific facts over one long sentence.
- If the turn names a thing the user might later ask about by that name \
  (hotel name, flight number, place, doctor, supplier, restaurant), it \
  MUST appear in `facts`. The narrative alone won't survive recall.
- If a turn truly has no concrete facts (pure feelings, greetings), use `[]`.

Respond with the JSON array only — no prose, no markdown fences.";

/// Build the user-turn payload for the extractor, prefixed with the absolute
/// conversation date. Giving the model the anchor — and instructing it (in the
/// system prompt) to resolve relative time expressions against it — is what
/// stops "last night" / "yesterday" being stored verbatim and then re-read
/// against the wrong day weeks later.
fn extraction_user_payload(
    event_time: chrono::DateTime<chrono::Utc>,
    agent_name: &str,
    user_message: &str,
    agent_response: &str,
) -> String {
    let when = event_time.format("%A %Y-%m-%d").to_string();
    format!(
        "## Conversation date\n\n{when} (UTC). Resolve every relative time reference in the \
         narrative (today, yesterday, last night, this morning, \"on Thursday\") to an absolute \
         calendar date anchored to THIS date.\n\n\
         ## Who is who\n\n\
         The assistant is called \"{agent_name}\" — that is you, the one writing this memory. \
         Everything under \"Assistant response\" is yours. The user is a different person and \
         is never called \"{agent_name}\"; if that name appears in the exchange it refers to \
         you or to something named after you, never to the user.\n\n\
         ## User message\n\n{user_message}\n\n## Assistant response\n\n{agent_response}"
    )
}

/// One extracted event as produced by the LLM.
#[derive(Debug, Deserialize)]
struct ExtractedEvent {
    content: String,
    /// Concrete facts pinned in the turn (names, dates, places, IDs). Appended
    /// to `content` at storage time so vector recall sees the entity strings
    /// directly — narrative-only memories lose entity recall under cosine
    /// similarity. Optional for backwards compatibility with older extractor
    /// outputs that pre-date the fact-grade prompt.
    #[serde(default)]
    facts: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_importance")]
    importance: f32,
    #[serde(default)]
    emotion: Option<String>,
}

fn default_importance() -> f32 {
    0.5
}

/// Append `facts` to the narrative `content` as a trailing `Facts:` block.
/// Vector recall scores the whole content string, so naming a hotel/flight/
/// ticket in `facts` makes that memory retrievable by that name. Returns
/// `content` unchanged when `facts` is empty so old-shape memories don't
/// gain a dangling header.
fn compose_content_with_facts(content: &str, facts: &[String]) -> String {
    let cleaned: Vec<&str> = facts
        .iter()
        .map(|f| f.trim())
        .filter(|f| !f.is_empty())
        .collect();
    if cleaned.is_empty() {
        return content.to_string();
    }
    let mut out = content.trim_end().to_string();
    out.push_str("\n\nFacts:\n");
    for fact in cleaned {
        out.push_str("- ");
        out.push_str(fact);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Parse the LLM response into a list of events. Returns `None` only when
/// nothing JSON-shaped can be salvaged; the caller then drops the output —
/// zero-JSON responses are model waffle, and storing them verbatim polluted
/// recall with raw prose blobs.
///
/// Tolerated shapes, in order: bare array, fenced array, prose-wrapped array
/// (e.g. "Here are the events: [...]"), object containing the array (e.g.
/// `{"events": [...]}`), a lone object promoted into a one-element array,
/// and a token-cap-truncated array salvaged object-by-object.
fn parse_events(raw: &str) -> Option<Vec<ExtractedEvent>> {
    let body = strip_code_fence(raw.trim());

    if let Ok(events) = serde_json::from_str::<Vec<ExtractedEvent>>(body) {
        return Some(events);
    }
    if let Some(array_slice) = first_bracketed_array(body) {
        if let Ok(events) = serde_json::from_str::<Vec<ExtractedEvent>>(array_slice) {
            return Some(events);
        }
    }
    if let Ok(event) = serde_json::from_str::<ExtractedEvent>(body) {
        return Some(vec![event]);
    }
    // Truncated output (the LLM hit its token cap mid-array) has no closing
    // `]`, so the paths above all fail. Salvage every complete object rather
    // than letting the caller dump the raw fenced fragment into the store as
    // one giant unreadable memory.
    let salvaged: Vec<ExtractedEvent> = complete_objects_of_truncated_array(body)
        .into_iter()
        .filter_map(|obj| serde_json::from_str::<ExtractedEvent>(obj).ok())
        .collect();
    if !salvaged.is_empty() {
        return Some(salvaged);
    }
    None
}

/// Run passive memory extraction for a single conversation turn.
///
/// Returns the number of memories written (0 if the turn was routine, gated, or
/// produced no extractable events).
///
/// `channel` is the session channel — used to skip background-agent turns
/// (memory consolidation, scheduler tasks). Pass an empty string when called
/// from a context that has no channel (e.g. the session-scanner backfill);
/// the gate then falls through to the recap-detection check.
///
/// `event_time` anchors the depicted event to wallclock — for live turns it's
/// the latest source-message timestamp; for session-scanner backfill it's the
/// archived turn's timestamp. Pass `None` to fall back to "now", which is
/// only correct for turns that just happened.
#[allow(clippy::too_many_arguments)]
pub async fn extract_and_store(
    llm: Arc<dyn LlmProvider>,
    store: Arc<dyn MemoryStore>,
    agent_id: AgentId,
    channel: &str,
    session_id: Option<String>,
    message_ids: Vec<String>,
    event_time: Option<chrono::DateTime<chrono::Utc>>,
    user_message: &str,
    agent_response: &str,
    model: Option<String>,
    custom_prompt: Option<String>,
    cost_tracker: Option<Arc<CostTracker>>,
) -> Result<usize> {
    match gate(channel, user_message) {
        ExtractionGate::Proceed => {}
        skip => {
            debug!(
                channel = channel,
                reason = skip.reason(),
                "Passive memory extraction skipped"
            );
            return Ok(0);
        }
    }

    // Resolve identity before the LLM call — a missing scope (DW-520) must
    // skip at zero cost, not after burning an extraction request.
    let identity_id = match crate::user_config::current_identity_id() {
        Some(uid) => uid,
        None => {
            tracing::warn!(
                channel = channel,
                "Skipping passive memory extraction: no CURRENT_USER_ID in scope (DW-520) — \
                 spawn site likely missing runner::spawn_in_session_context"
            );
            return Ok(0);
        }
    };

    let system_prompt = custom_prompt.as_deref().unwrap_or(DEFAULT_PASSIVE_PROMPT);

    // Anchor the extracted narrative to wall-clock. For live turns this is the
    // latest source-message timestamp; for session-scanner backfill it's the
    // archived turn's timestamp. The payload carries this date so the extractor
    // resolves relative phrasing ("last night") to absolute dates at write time.
    let resolved_event_time = event_time.unwrap_or_else(chrono::Utc::now);
    let user_msg = extraction_user_payload(
        resolved_event_time,
        agent_id.as_str(),
        user_message,
        agent_response,
    );

    // Suppress extended reasoning: this is a single-shot JSON extraction, and
    // reasoning models otherwise burn the whole token budget on hidden
    // reasoning and return empty/truncated content (dropping the memory).
    // 2048 tokens: multi-event turns overflowed 1024 and arrived truncated,
    // leaving `parse_events` only its object-by-object salvage path.
    let (response, served) = with_served_log(
        workforce_agent::llm::provider::with_reasoning_suppressed(llm.create_message(
            Some(system_prompt),
            &[LlmMessage::user(user_msg)],
            &[],
            model.as_deref(),
            Some(2048),
            true,
        )),
    )
    .await;
    let response =
        response.map_err(|e| eyre::eyre!("Passive memory extraction LLM call failed: {}", e))?;

    if let Some(ref ct) = cost_tracker {
        ct.record(CostEntry {
            task_type: Some("passive_memory"),
            description: Some("memory:passive_extraction"),
            served,
            ..CostEntry::new(
                agent_id.as_str(),
                model.as_deref().unwrap_or("unknown"),
                TokenCounts::from(&response.usage),
                Origin::PassiveMemory,
            )
        });
    }

    let raw = extract_text_from_response(&response.content);
    let trimmed = raw.trim();

    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("NONE") || trimmed == "[]" {
        debug!("Passive memory extraction: nothing to remember");
        return Ok(0);
    }

    let Some(events) = parse_events(trimmed) else {
        // `parse_events` already tolerates six output shapes; whatever is
        // left is model waffle, not signal. Storing it verbatim polluted
        // recall with unreadable prose blobs — drop it, keep a diagnosable
        // trace of what the model actually said.
        let head: String = trimmed.chars().take(160).collect();
        warn!(
            raw_len = trimmed.len(),
            raw_head = %head,
            "Passive extraction output did not parse as JSON events; dropping"
        );
        return Ok(0);
    };

    if events.is_empty() {
        return Ok(0);
    }

    // The `date:` tag is derived from event_time so it survives lazy
    // backfills correctly: a session-scanner re-extracting last week's turn
    // produces a memory tagged `date:<last-week>`, not today.
    let date = resolved_event_time.format("%Y-%m-%d").to_string();

    let mut stored = 0usize;
    let mut merged = 0usize;
    for event in events {
        let memory = build_memory(
            &agent_id,
            &date,
            resolved_event_time,
            Some(identity_id.as_str()),
            &session_id,
            &message_ids,
            event,
        );
        match remember_with_dedup(store.as_ref(), memory, passive_dedup_config()).await {
            Ok(WriteOutcome::Inserted) => stored += 1,
            Ok(WriteOutcome::MergedInto {
                recurrence_count, ..
            }) => {
                merged += 1;
                debug!(
                    recurrence_count,
                    "Near-duplicate of an existing memory — merged into recurrence"
                );
            }
            Err(e) => warn!(error = %e, "Failed to store extracted event memory"),
        }
    }

    if stored > 0 || merged > 0 {
        info!(
            inserted = stored,
            merged_recurrences = merged,
            "Passive memory extraction: stored events"
        );
    }
    Ok(stored + merged)
}

fn passive_dedup_config() -> DedupConfig {
    DedupConfig {
        layer: MemoryLayer::Episodic,
        source: None,
        window: PASSIVE_DEDUP_WINDOW,
        // Keep the strict 0.92 bar (unlike lessons, which dropped to 0.80).
        // Measured on the prod corpus: passive narrative prose embeds densely —
        // *distinct* turn-events about one topic already reach 0.88-0.90, while
        // genuine near-identical re-extractions sit at ~0.92+. 0.92 is the only
        // bar that separates them. Any lower silently merges distinct events,
        // and a merge DROPS the new memory (see remember_with_dedup), so an
        // over-merge is irreversible episodic-history loss — far worse than a
        // missed dup, which consolidation handles anyway.
        near_duplicate_similarity: DEFAULT_NEAR_DUPLICATE_SIMILARITY,
    }
}

fn build_memory(
    agent_id: &AgentId,
    date: &str,
    event_time: chrono::DateTime<chrono::Utc>,
    identity_id: Option<&str>,
    session_id: &Option<String>,
    message_ids: &[String],
    event: ExtractedEvent,
) -> Memory {
    let mut tags = event.tags;
    tags.push("source:passive".to_string());
    tags.push(format!("date:{date}"));
    if let Some(e) = event.emotion.as_ref().filter(|s| !s.is_empty()) {
        tags.push(format!("emotion:{e}"));
    }

    let content = compose_content_with_facts(&event.content, &event.facts);

    let mut memory = Memory::new(content, MemoryLayer::Episodic, agent_id.clone())
        .with_source(MemorySource::Passive)
        .with_importance(event.importance)
        .with_event_time(event_time)
        .with_tags(tags);
    if let Some(uid) = identity_id {
        memory = memory.with_identity_id(uid);
    }
    if let Some(sid) = session_id.as_ref().filter(|s| !s.is_empty()) {
        memory = memory.with_source_session(sid.clone(), message_ids.to_vec());
    }
    memory
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure this prevents: a stored note read "On 2026-08-15, Allan
    /// decided to test my suggestion…" — Allan is the *agent*, and the note had
    /// given the agent's own name to the user. The extractor was never told who
    /// either party was, so it inferred from whatever names appeared in the
    /// text. A memory with the roles swapped is worse than none: it is read
    /// back as fact and is indistinguishable from a true one.
    #[test]
    fn the_payload_says_which_name_belongs_to_the_assistant() {
        let when = chrono::Utc::now();
        let payload = extraction_user_payload(when, "allan", "what shall we do", "let us test it");
        assert!(payload.contains("Who is who"), "{payload}");
        assert!(
            payload.contains("never called \"allan\""),
            "the user must be excluded from the agent's name: {payload}"
        );
    }

    /// And the instruction has to survive a turn where the agent's name is
    /// spoken by the user, which is exactly when the confusion happened.
    #[test]
    fn the_agents_name_appearing_in_the_turn_is_still_attributed_to_the_agent() {
        let when = chrono::Utc::now();
        let payload = extraction_user_payload(when, "allan", "allan, play some music", "playing");
        assert!(
            payload.contains(
                "refers to \
             you or to something named after you, never to the user"
            ) || payload.contains("never to the user"),
            "{payload}"
        );
    }

    #[test]
    fn extraction_payload_anchors_conversation_date() {
        let when = chrono::DateTime::parse_from_rfc3339("2026-06-01T20:54:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let payload = extraction_user_payload(
            when,
            "allan",
            "we read a bedtime story",
            "I narrated Leo's tale",
        );
        // The absolute event date is present and the model is told to resolve
        // relative phrasing against it.
        assert!(payload.contains("2026-06-01"), "missing date: {payload}");
        assert!(payload.contains("Resolve every relative time reference"));
        // Both halves of the turn survive.
        assert!(payload.contains("## User message"));
        assert!(payload.contains("we read a bedtime story"));
        assert!(payload.contains("## Assistant response"));
        assert!(payload.contains("I narrated Leo's tale"));
    }

    #[test]
    fn parse_events_accepts_plain_json_array() {
        let raw = r#"[
            {"content": "a", "tags": ["x"], "importance": 0.8, "emotion": "proud"},
            {"content": "b"}
        ]"#;
        let events = parse_events(raw).expect("should parse");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].content, "a");
        assert_eq!(events[0].tags, vec!["x"]);
        assert_eq!(events[0].importance, 0.8);
        assert_eq!(events[0].emotion.as_deref(), Some("proud"));
        assert!(events[0].facts.is_empty());
        assert_eq!(events[1].importance, default_importance());
        assert!(events[1].tags.is_empty());
        assert!(events[1].emotion.is_none());
        assert!(events[1].facts.is_empty());
    }

    /// Token-cap truncation (the source of the raw `` ```json [ {...`` blob
    /// memories on driftwerk-prod, 2026-07-04): a fenced array cut mid-object
    /// must yield its complete leading objects, not fall through to the
    /// store-raw path.
    #[test]
    fn parse_events_salvages_truncated_fenced_array() {
        let raw = "```json\n[\n  {\"content\": \"On 2026-07-04, the user shared X\", \"tags\": [\"a\"]},\n  {\"content\": \"On 2026-07-04, the user shared Y\"},\n  {\"content\": \"On 2026-07-04, the us";
        let events = parse_events(raw).expect("should salvage complete objects");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].content, "On 2026-07-04, the user shared X");
        assert_eq!(events[1].content, "On 2026-07-04, the user shared Y");
    }

    #[test]
    fn parse_events_still_none_when_nothing_salvageable() {
        assert!(parse_events("just some prose with no JSON").is_none());
        assert!(parse_events("```json\n[ {\"content\": \"cut mid").is_none());
    }

    #[test]
    fn parse_events_captures_facts_field() {
        let raw = r#"[{
            "content": "We talked about the Uganda trip.",
            "facts": ["person:Anne (girlfriend, from Uganda)", "location:Kampala",
                      "hotel:Speke Resort Munyonyo"],
            "tags": ["travel"]
        }]"#;
        let events = parse_events(raw).expect("should parse with facts");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].facts.len(), 3);
        assert_eq!(events[0].facts[2], "hotel:Speke Resort Munyonyo");
    }

    #[test]
    fn compose_content_appends_facts_block() {
        let out = compose_content_with_facts(
            "Kings asked about the Uganda trip.",
            &[
                "person:Anne (girlfriend)".to_string(),
                "hotel:Speke Resort Munyonyo".to_string(),
            ],
        );
        assert!(out.starts_with("Kings asked about the Uganda trip."));
        assert!(out.contains("Facts:"));
        assert!(out.contains("- person:Anne (girlfriend)"));
        assert!(out.contains("- hotel:Speke Resort Munyonyo"));
    }

    #[test]
    fn compose_content_no_facts_returns_content_unchanged() {
        let out = compose_content_with_facts("hello", &[]);
        assert_eq!(out, "hello");
    }

    #[test]
    fn compose_content_drops_empty_facts() {
        let out = compose_content_with_facts(
            "x",
            &["".to_string(), "  ".to_string(), "person:Anne".to_string()],
        );
        assert!(out.contains("- person:Anne"));
        // Only one fact line should appear — empties filtered out.
        assert_eq!(out.matches("- ").count(), 1);
    }

    #[test]
    fn parse_events_strips_code_fence() {
        let raw = "```json\n[{\"content\": \"x\"}]\n```";
        let events = parse_events(raw).expect("should strip fences");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "x");
    }

    #[test]
    fn parse_events_returns_none_for_non_json() {
        assert!(parse_events("just some prose").is_none());
        assert!(parse_events("NONE").is_none());
    }

    #[test]
    fn parse_events_handles_empty_array() {
        let events = parse_events("[]").expect("should parse empty");
        assert!(events.is_empty());
    }

    #[test]
    fn parse_events_recovers_array_from_prose_preamble() {
        let raw =
            "Here are the events I extracted:\n[{\"content\": \"x\"}, {\"content\": \"y\"}]\nLet me know if you want changes.";
        let events = parse_events(raw).expect("should recover array from prose");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].content, "x");
        assert_eq!(events[1].content, "y");
    }

    #[test]
    fn parse_events_recovers_array_from_object_wrapper() {
        let raw = r#"{"events": [{"content": "x", "importance": 0.9}]}"#;
        let events = parse_events(raw).expect("should pull array out of object wrapper");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "x");
        assert_eq!(events[0].importance, 0.9);
    }

    #[test]
    fn parse_events_promotes_single_object_to_array() {
        let raw = r#"{"content": "x", "tags": ["a"], "importance": 0.6}"#;
        let events = parse_events(raw).expect("should promote single object");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "x");
        assert_eq!(events[0].tags, vec!["a"]);
        assert_eq!(events[0].importance, 0.6);
    }

    #[test]
    fn parse_events_salvages_complete_objects_when_array_is_truncated() {
        // MaxTokens cutoffs leave dangling JSON — salvage the complete
        // leading objects instead of making the caller store the raw
        // fragment as one unreadable memory.
        let raw = "[{\"content\": \"x\"}, {\"content\": \"y";
        let events = parse_events(raw).expect("should salvage the complete object");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "x");
    }

    #[test]
    fn build_memory_populates_tags_identity_and_source_link() {
        let event = ExtractedEvent {
            content: "User ate too much chicken and I advised water.".to_string(),
            facts: Vec::new(),
            tags: vec!["meal".to_string(), "overeating".to_string()],
            importance: 0.7,
            emotion: Some("regret".to_string()),
        };
        let event_time = chrono::DateTime::parse_from_rfc3339("2026-04-17T14:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let memory = build_memory(
            &AgentId::new("atlas"),
            "2026-04-17",
            event_time,
            Some("user-abc"),
            &Some("session-xyz".to_string()),
            &["msg-1".to_string(), "msg-2".to_string()],
            event,
        );

        assert_eq!(
            memory.content,
            "User ate too much chicken and I advised water."
        );
        assert_eq!(memory.importance, 0.7);
        assert_eq!(memory.identity_id.as_deref(), Some("user-abc"));
        assert_eq!(memory.source_session_id.as_deref(), Some("session-xyz"));
        assert_eq!(memory.source_message_ids, vec!["msg-1", "msg-2"]);
        assert_eq!(memory.source, MemorySource::Passive);
        assert_eq!(memory.event_time, event_time);
        assert!(memory.tags.contains(&"meal".to_string()));
        assert!(memory.tags.contains(&"overeating".to_string()));
        assert!(memory.tags.contains(&"source:passive".to_string()));
        assert!(memory.tags.contains(&"date:2026-04-17".to_string()));
        assert!(memory.tags.contains(&"emotion:regret".to_string()));
    }

    use async_trait::async_trait;
    use workforce_agent::llm::types::{
        ContentBlock, LlmMessage as TestLlmMessage, LlmResponse, LlmToolDef, StopReason, TokenUsage,
    };
    use workforce_memory::in_memory::InMemoryStore;
    use workforce_memory::types::MemoryFilter;

    struct PanicLlm;

    #[async_trait]
    impl LlmProvider for PanicLlm {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[TestLlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            panic!("extraction must not reach the LLM on this path");
        }
    }

    struct ProseLlm;

    #[async_trait]
    impl LlmProvider for ProseLlm {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[TestLlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: "I reviewed the conversation and found nothing structured to report."
                        .to_string(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: "mock".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn missing_identity_skips_before_the_llm_call() {
        let stored = extract_and_store(
            Arc::new(PanicLlm),
            Arc::new(InMemoryStore::new()),
            AgentId::new("allan"),
            "ios",
            None,
            Vec::new(),
            None,
            "hi there",
            "hello!",
            None,
            None,
            None,
        )
        .await
        .expect("skip is not an error");
        assert_eq!(stored, 0);
    }

    #[tokio::test]
    async fn unparseable_extraction_output_is_dropped_not_stored() {
        let store = Arc::new(InMemoryStore::new());
        let stored = workforce_agent::runner::CURRENT_USER_ID
            .scope(
                "user-1".to_string(),
                extract_and_store(
                    Arc::new(ProseLlm),
                    store.clone(),
                    AgentId::new("allan"),
                    "ios",
                    None,
                    Vec::new(),
                    None,
                    "we discussed the trip",
                    "noted — sounds exciting",
                    None,
                    None,
                    None,
                ),
            )
            .await
            .expect("unparseable output is dropped, not an error");
        assert_eq!(stored, 0);
        let remaining = store.list(MemoryFilter::default()).await.unwrap();
        assert!(
            remaining.is_empty(),
            "raw prose must not be minted as a memory"
        );
    }

    #[test]
    fn build_memory_omits_session_link_when_absent() {
        let event = ExtractedEvent {
            content: "c".to_string(),
            facts: Vec::new(),
            tags: vec![],
            importance: 0.3,
            emotion: None,
        };
        let event_time = chrono::Utc::now();
        let memory = build_memory(
            &AgentId::new("a"),
            "2026-04-17",
            event_time,
            None,
            &None,
            &[],
            event,
        );
        assert!(memory.identity_id.is_none());
        assert!(memory.source_session_id.is_none());
        assert!(memory.source_message_ids.is_empty());
        // No emotion tag when emotion is None.
        assert!(memory.tags.iter().all(|t| !t.starts_with("emotion:")));
    }
}

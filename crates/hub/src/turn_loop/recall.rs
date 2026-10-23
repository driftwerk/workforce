//! What a turn recalls: the memory query, its ranking and what is held back.

/// Merge two recall result lists by memory id, keeping the higher score per
/// id. Used by the context-fused auto-inject when both the literal and fused
/// queries run — a memory surfaced by either path should win on the better
/// score, not be silently dropped as a duplicate.
pub(crate) fn dedupe_recall_keep_best(
    results: Vec<workforce_memory::types::MemorySearchResult>,
) -> Vec<workforce_memory::types::MemorySearchResult> {
    use std::collections::HashMap;
    let mut best: HashMap<String, workforce_memory::types::MemorySearchResult> = HashMap::new();
    for r in results {
        let key = r.memory.id.0.clone();
        match best.get(&key) {
            Some(existing) if existing.score >= r.score => {}
            _ => {
                best.insert(key, r);
            }
        }
    }
    best.into_values().collect()
}

/// Bounded multiplicative recency boost on recall scores, anchored on
/// `event_time` (never `created_at` — a scanner backfill of last week's turn
/// must read as old; see `Memory::event_time`).
/// `score' = min(1.0, score * (1 + boost * 0.5^(age_hours / half_life_hours)))`.
/// `boost <= 0` or `half_life <= 0` → no-op. Applies only to the generic
/// `## Recalled Memories` block — lessons/training/self-learned recall have
/// separately calibrated thresholds that a boost would silently detune.
pub(crate) fn apply_recall_recency_boost(
    results: &mut [workforce_memory::types::MemorySearchResult],
    now: chrono::DateTime<chrono::Utc>,
    boost: f32,
    half_life_hours: f32,
) {
    if boost <= 0.0 || half_life_hours <= 0.0 {
        return;
    }
    for r in results.iter_mut() {
        let age_hours = ((now - r.memory.event_time).num_seconds().max(0) as f32) / 3600.0;
        let multiplier =
            1.0 + boost * (-age_hours / half_life_hours * std::f32::consts::LN_2).exp();
        r.score = (r.score * multiplier).min(1.0);
    }
}

/// Compose a context-fused recall query from the tail of a session and the
/// current user message. Pure function — see `AgentService::build_fused_recall_query`
/// for the call site. Returns `None` when the tail has no usable
/// user/assistant content.
///
/// Why this exists: vector recall against a pronoun-heavy turn like
/// "you should know where I'm heading" has near-zero overlap with stored
/// memories about "Uganda" or "Kampala hotel". Concatenating the recent
/// assistant turns (which often name the entity) into the query restores
/// that overlap. The literal-message recall still runs in parallel; both
/// result sets are unioned.
pub(crate) fn fuse_recall_query(
    messages: &[workforce_common::session::SessionMessage],
    user_message: &str,
) -> Option<String> {
    use workforce_common::session::MessageRole;

    const PER_MSG_CHARS: usize = 400;
    const MAX_FUSED_CHARS: usize = 1600;
    const TAIL_DEPTH: usize = 6;

    if messages.is_empty() {
        return None;
    }

    let mut budget = MAX_FUSED_CHARS;
    let mut tail: Vec<String> = messages
        .iter()
        .rev()
        .take(TAIL_DEPTH)
        .filter(|m| matches!(m.role, MessageRole::User | MessageRole::Assistant))
        .map(|m| m.content.chars().take(PER_MSG_CHARS).collect::<String>())
        .filter(|s| !s.trim().is_empty())
        .take_while(|s| {
            if s.len() > budget {
                return false;
            }
            budget -= s.len();
            true
        })
        .collect();
    if tail.is_empty() {
        return None;
    }
    tail.reverse();
    tail.push(user_message.chars().take(PER_MSG_CHARS).collect());
    Some(tail.join("\n"))
}

/// Persist the lesson IDs and scores injected this turn onto the in-scope
/// session's metadata, so the post-turn reinforcement read sees them after
/// the caller's `sessions.save(&session)` lands. Writing to a separate
/// metadata path (rather than relying on the build-prompt function to save
/// its own copy) avoids the save-race where the post-turn handler's later
/// `sessions.save(&session)` clobbers an earlier metadata write.
pub(crate) fn stash_injected_lessons(
    session: &mut workforce_common::session::Session,
    lessons: &[crate::lessons::InjectedLesson],
) {
    if lessons.is_empty() {
        return;
    }
    if !session.metadata.is_object() {
        session.metadata = serde_json::json!({});
    }
    if let (Some(map), Ok(value)) = (
        session.metadata.as_object_mut(),
        serde_json::to_value(lessons),
    ) {
        map.insert("injected_lessons".to_string(), value);
    }
}

/// Whether prompt-level recall injection — lessons, training recall,
/// self-learned/deep-expertise, generic memories — is suppressed for this
/// turn: always for a mechanical workflow-step executor (chat-behavior
/// guidance in a call-tool-emit-OUTPUTS prompt is cost and distraction),
/// and for time-scoped recap questions, where vector recall surfaces
/// consolidated memories the LLM conflates with the requested window.
pub(crate) fn recall_injection_suppressed(
    mechanical_session: bool,
    user_message: Option<&str>,
) -> bool {
    mechanical_session
        || user_message
            .map(crate::passive_memory::looks_like_recap_request)
            .unwrap_or(false)
}

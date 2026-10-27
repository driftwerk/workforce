//! Threaded session rollover.
//!
//! A "thread" is the set of sessions sharing `(channel, owner)`; the active one
//! is the newest unsealed member (see `SessionStore::get_or_create`). When the
//! active session approaches the compaction trigger, rollover seals it and mints
//! a fresh successor so the working context stays small and fast. The prior
//! conversation is preserved both verbatim (recent turns) and as a carry-over
//! summary, keeping continuity across the seam.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::Result;

use workforce_common::session::{
    estimate_tokens, CompactionConfig, MessageRole, ReservedContext, Session, SessionMessage,
    SessionStore,
};

use crate::compaction::CompactionEngine;

/// Prefix marking the seeded carry-over summary message on a successor session.
pub const CARRYOVER_PREFIX: &str = "[Thread carry-over summary]";

/// Recent turns carried verbatim across a rollover for voice sessions —
/// larger than the text default because a flowing spoken conversation needs
/// more immediate context to stay coherent across the seam.
const VOICE_ROLLOVER_KEEP_RECENT: usize = 12;

/// Conversational messages quoted in a stale-resume carry-over, and the
/// per-message clip. Enough to recognise the old topic, deliberately too
/// little to resume it as if current.
const STALE_CARRYOVER_RECENT: usize = 6;
const STALE_CARRYOVER_CLIP: usize = 240;

/// Decide whether `session` should roll over now: rollover pressure has built
/// up AND there is enough compactable history to be worth summarising. The
/// compactable-count check is the loop-guard — it also rejects brand-new and
/// near-empty sessions so rollover never mints an empty successor. Sealed or
/// mid-rollover sessions never roll.
pub fn should_roll(
    session: &Session,
    config: &CompactionConfig,
    reserved: ReservedContext,
    compactable_count: usize,
) -> bool {
    if session.is_sealed() || session.rollover_pending() {
        return false;
    }
    if !rollover_pressure(session, config, reserved) {
        return false;
    }
    compactable_count >= config.min_messages_to_compact
}

/// Whether either rollover trigger has fired: the token watermark, or the
/// conversational message-count ceiling. Cheap (no engine), so callers use
/// it as a pre-check before building a [`CompactionEngine`].
pub fn rollover_pressure(
    session: &Session,
    config: &CompactionConfig,
    reserved: ReservedContext,
) -> bool {
    crossed_watermark(session, config, reserved) || count_pressure(session, config)
}

/// Conversational-channel turns are tiny, so a chat thread degrades on
/// message volume long before the token watermark is reachable. Voice
/// sessions roll at `voice_rollover_max_messages` (spoken context decays
/// fastest); text sessions on conversational channels at the looser
/// `chat_rollover_max_messages`. 0 disables either ceiling; platform-scoped
/// channels (Slack threads etc.) have no count ceiling.
pub fn count_pressure(session: &Session, config: &CompactionConfig) -> bool {
    let ceiling = if session.is_voice() {
        config.voice_rollover_max_messages
    } else if workforce_common::channels::is_conversational(&session.channel) {
        config.chat_rollover_max_messages
    } else {
        0
    };
    ceiling > 0 && session.conversational_message_count() >= ceiling
}

/// Whether the conversation has consumed `rollover_warning_ratio` of its
/// budget — the compaction trigger minus the fixed reserved overhead. This is
/// framed against the conversation (non-system) tokens rather than the total,
/// because the reserved floor (system prompt + tools + response) is constant
/// and can itself exceed `trigger * ratio`; keying on the total would fire
/// rollover on an essentially empty session. Cheap (no engine), so callers use
/// it as a pre-check before building a [`CompactionEngine`].
pub fn crossed_watermark(
    session: &Session,
    config: &CompactionConfig,
    reserved: ReservedContext,
) -> bool {
    let budget = config.trigger_threshold().saturating_sub(reserved.total());
    if budget == 0 {
        // Reserved overhead alone meets the trigger; a fresh session would
        // inherit the same floor, so rolling can't create headroom.
        return false;
    }
    let watermark = (budget as f32 * config.rollover_warning_ratio) as u32;
    session.non_system_message_tokens() >= watermark
}

/// Recent turns to carry verbatim across a rollover: voice sessions get a
/// larger window than the text default so a flowing spoken conversation keeps
/// enough immediate context to stay coherent across the seam.
fn keep_recent_for(session: &Session, config: &CompactionConfig) -> usize {
    if session.is_voice() {
        VOICE_ROLLOVER_KEEP_RECENT
    } else {
        config.rollover_keep_recent
    }
}

/// Decide and perform a rollover end-to-end, spawning the async carry-over
/// seeding. A no-op when [`should_roll`] is false. Callers should gate the call
/// on the cheap watermark pre-check (`estimated_total_with_reserved >=
/// rollover_watermark`) so an engine is only built when a rollover is plausible.
pub fn maybe_roll_over(
    store: Arc<dyn SessionStore>,
    engine: CompactionEngine,
    session: &mut Session,
    config: &CompactionConfig,
    reserved: ReservedContext,
) {
    let keep = keep_recent_for(session, config);
    let compactable = engine.compactable_message_count(session, keep);
    if !should_roll(session, config, reserved, compactable) {
        return;
    }
    let trigger = if crossed_watermark(session, config, reserved) {
        "watermark"
    } else {
        "message_count"
    };
    match seal_and_mint(store.as_ref(), session, keep) {
        Ok(next) => {
            tracing::info!(
                sealed = %session.id,
                successor = %next.id,
                seq = next.seq(),
                channel = %session.channel,
                trigger,
                conversational_messages = session.conversational_message_count(),
                "Thread rollover: sealed session, minted successor"
            );
            let sealed_id = session.id.clone();
            let new_id = next.id.clone();
            tokio::spawn(seed_or_unwedge(store, engine, sealed_id, new_id, keep));
        }
        Err(e) => {
            tracing::warn!(error = %e, session = %session.id, "Rollover seal/mint failed; staying on current session");
        }
    }
}

/// Run the async carry-over seeding; if it fails, clear the successor's
/// `rollover_pending` flag anyway. A permanently set flag suppresses every
/// future watermark and stale rollover on the thread — reproducing the
/// unbounded-replay failure this module exists to prevent — so an unseeded
/// successor (it still carries the recent verbatim turns) beats a wedged one.
async fn seed_or_unwedge(
    store: Arc<dyn SessionStore>,
    engine: CompactionEngine,
    sealed_id: String,
    new_id: String,
    keep_recent: usize,
) {
    let Err(e) = seed_summary(
        Arc::clone(&store),
        engine,
        sealed_id,
        new_id.clone(),
        keep_recent,
    )
    .await
    else {
        return;
    };
    tracing::warn!(
        error = %e,
        successor = %new_id,
        "Rollover summary seeding failed; clearing rollover_pending so the thread can roll again"
    );
    match store.get(&new_id) {
        Ok(Some(mut next)) => {
            next.set_rollover_pending(false);
            if let Err(e) = store.save(&next) {
                tracing::warn!(error = %e, successor = %new_id, "Failed to clear rollover_pending");
            }
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %e, successor = %new_id, "Failed to load successor to clear rollover_pending");
        }
    }
}

/// Build the successor for `current`: same thread identity (label, context
/// type, model override, conversation mode), `seq + 1`, `prev` set, and the
/// last `keep_recent` messages carried verbatim. Pure construction — sealing
/// and persistence stay with the caller.
fn mint_successor(current: &Session, keep_recent: usize) -> Session {
    let mut next = current.successor();
    next.set_prev_session_id(&current.id);
    next.context_type = current.context_type.clone();
    next.model_override = current.model_override.clone();
    next.adopt_conversation_mode(current);
    // An interactive playbook outlives the session that started it: carry its
    // progress to the successor or the flow would silently die the first time
    // an implement step grows the context past the rollover threshold.
    if let Some(progress) = current.playbook() {
        next.set_playbook(&progress);
    }
    next.messages = current.recent_messages(keep_recent).to_vec();
    next.total_tokens = next.estimated_total_tokens() as u64;
    next
}

/// Synchronous phase: seal `current`, mint its successor (`seq + 1`, `prev`
/// set, `rollover_pending`), carry the last `keep_recent` messages verbatim for
/// immediate continuity, and persist both. Returns the new active session. The
/// carry-over summary is seeded afterwards by [`seed_summary`].
pub fn seal_and_mint(
    store: &dyn SessionStore,
    current: &mut Session,
    keep_recent: usize,
) -> Result<Session> {
    current.seal();

    let mut next = mint_successor(current, keep_recent);
    next.set_rollover_pending(true);

    // Seal + insert as one step so a store with a "one active session"
    // invariant never transiently sees zero or two unsealed rows.
    store.seal_and_insert_successor(current, &next)?;
    Ok(next)
}

/// A User/Assistant message that is genuine dialogue, not an inherited
/// carry-over. Staleness and the quoted tail both anchor on these: a
/// successor whose only message is a carry-over (stamped with the old
/// conversation's dates in its *content*) must never itself read as stale,
/// or every resume of a dormant thread would mint another successor.
fn is_real_dialogue(message: &SessionMessage) -> bool {
    matches!(message.role, MessageRole::User | MessageRole::Assistant)
        && !message.content.starts_with(CARRYOVER_PREFIX)
}

/// Whether any genuine dialogue has happened in this session — false for a
/// brand-new session and for a stale-rollover successor still holding only
/// its carry-over. The seam session-open hooks (welcome, resume context)
/// key on.
pub fn has_real_dialogue(session: &Session) -> bool {
    session.messages.iter().any(is_real_dialogue)
}

/// Timestamp of the last genuine conversational exchange, if any. Deliberately
/// not `updated_at` — metadata writes (voice-mode flags) bump `updated_at`
/// without any conversation happening.
fn last_dialogue_at(session: &Session) -> Option<DateTime<Utc>> {
    session
        .messages
        .iter()
        .rev()
        .find(|m| is_real_dialogue(m))
        .map(|m| m.timestamp)
}

/// Whether resuming `session` now would replay a conversation that ended
/// `stale_rollover_hours` ago (0 disables). Sealed or mid-rollover sessions
/// are never stale; sessions without genuine dialogue (empty, or seeded only
/// with a carry-over) have nothing to go stale.
pub fn is_stale(session: &Session, config: &CompactionConfig, now: DateTime<Utc>) -> bool {
    if config.stale_rollover_hours == 0 || session.is_sealed() || session.rollover_pending() {
        return false;
    }
    let threshold_secs =
        i64::try_from(config.stale_rollover_hours.saturating_mul(3600)).unwrap_or(i64::MAX);
    last_dialogue_at(session).is_some_and(|last| (now - last).num_seconds() >= threshold_secs)
}

/// Resume-time rollover: return `session` untouched while it is fresh;
/// once stale, seal it and return a successor seeded with a *dated*
/// carry-over so the model reads the old conversation as history, not as
/// what was just said. Deterministic and synchronous — no LLM call, no
/// async seeding, so the successor is complete before the first turn runs
/// (`rollover_pending` stays false). If persisting loses a race with a
/// concurrent resume of the same thread, the winner's active session is
/// re-resolved instead of failing the turn.
pub fn roll_if_stale(
    store: &dyn SessionStore,
    session: Session,
    config: &CompactionConfig,
    now: DateTime<Utc>,
) -> Result<Session> {
    if !is_stale(&session, config, now) {
        return Ok(session);
    }
    let carryover = stale_carryover_content(&session, now);
    roll_onto_carryover(store, session, carryover, now, "stale")
}

/// Resume-time rollover for a session that automated runs keep adding to,
/// such as the one a scheduled job is pinned to. Each run is small, so the
/// token thresholds are no bound on it; once it replays `ceiling` messages
/// (0 disables) it is sealed and the run continues in a successor holding a
/// dated carry-over of the last exchanges, exactly as [`roll_if_stale`]
/// builds one.
pub fn roll_if_over_ceiling(
    store: &dyn SessionStore,
    session: Session,
    ceiling: usize,
    now: DateTime<Utc>,
) -> Result<Session> {
    let replayed = session
        .messages
        .iter()
        .filter(|m| m.role != MessageRole::System)
        .count();
    if ceiling == 0 || replayed < ceiling || session.is_sealed() || session.rollover_pending() {
        return Ok(session);
    }
    let carryover = format!(
        "{CARRYOVER_PREFIX}\nThis session holds the runs of one recurring job. Its {replayed} \
         earlier messages were set aside so each run stays small.\nFinal exchanges before \
         that:\n{}",
        dialogue_tail(&session)
    );
    roll_onto_carryover(store, session, carryover, now, "message_count")
}

/// Seal `session` and return its successor holding `carryover` alone.
fn roll_onto_carryover(
    store: &dyn SessionStore,
    mut sealed: Session,
    carryover: String,
    now: DateTime<Utc>,
    trigger: &'static str,
) -> Result<Session> {
    sealed.seal();

    let mut next = mint_successor(&sealed, 0);
    next.messages.push(SessionMessage {
        id: uuid::Uuid::new_v4().to_string(),
        role: MessageRole::User,
        tool_data: None,
        timestamp: now,
        tokens: estimate_tokens(&carryover),
        content: carryover,
        external_id: None,
        attachments: None,
        trust_level: None,
        actor: None,
    });
    next.total_tokens = next.estimated_total_tokens() as u64;

    match store.seal_and_insert_successor(&sealed, &next) {
        Ok(()) => {
            tracing::info!(
                sealed = %sealed.id,
                successor = %next.id,
                seq = next.seq(),
                channel = %sealed.channel,
                trigger,
                days_dormant = last_dialogue_at(&sealed)
                    .map(|t| (now - t).num_days())
                    .unwrap_or(0),
                "Resume rollover: sealed session, minted dated successor"
            );
            Ok(next)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                session = %sealed.id,
                trigger,
                "Resume rollover insert failed (concurrent resume?); re-resolving active session"
            );
            store.get_or_create(&sealed.channel, &sealed.owner, &sealed.agent_id)
        }
    }
}

/// The dated carry-over for a stale-resume rollover: when the previous
/// conversation ended, how long ago that was, and a clipped tail of its
/// final genuine exchanges — explicitly framed as historical.
fn stale_carryover_content(sealed: &Session, now: DateTime<Utc>) -> String {
    let ended = last_dialogue_at(sealed).unwrap_or(sealed.updated_at);
    let days = (now - ended).num_days().max(0);
    let ago = match days {
        0 => "earlier today".to_string(),
        1 => "1 day ago".to_string(),
        n => format!("{n} days ago"),
    };
    format!(
        "{CARRYOVER_PREFIX}\nThe previous conversation in this thread ended on {} ({ago}). \
         It is historical context only — do not treat its topics as current unless the user \
         raises them again.\nFinal exchanges of that conversation:\n{}",
        ended.format("%Y-%m-%d"),
        dialogue_tail(sealed)
    )
}

/// The clipped, dated tail of a session's final genuine exchanges. Inherited
/// carry-overs are excluded so successive rollovers never nest one
/// carry-over inside the next.
fn dialogue_tail(sealed: &Session) -> String {
    sealed
        .messages
        .iter()
        .filter(|m| is_real_dialogue(m))
        .rev()
        .take(STALE_CARRYOVER_RECENT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|m| {
            let who = match m.role {
                MessageRole::Assistant => "Assistant",
                _ => "User",
            };
            let date = m.timestamp.format("%Y-%m-%d");
            format!("[{date}] {who}: {}", clip(&m.content, STALE_CARRYOVER_CLIP))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Clip `text` to at most `max` characters on a char boundary, marking the cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…")
}

/// Asynchronous phase: summarise everything older than the carried-over recent
/// turns from the sealed session and prepend it onto the successor, then clear
/// `rollover_pending`. Prepending keeps the carry-over ahead of any turn that
/// may have landed mid-rollover.
pub async fn seed_summary(
    store: Arc<dyn SessionStore>,
    engine: CompactionEngine,
    sealed_id: String,
    new_id: String,
    keep_recent: usize,
) -> Result<()> {
    let sealed = store
        .get(&sealed_id)?
        .ok_or_else(|| eyre::eyre!("sealed session {sealed_id} disappeared before seeding"))?;
    let summary = engine
        .produce_carryover_summary(&sealed, keep_recent)
        .await?;

    let mut next = store
        .get(&new_id)?
        .ok_or_else(|| eyre::eyre!("successor session {new_id} disappeared before seeding"))?;

    let carryover = SessionMessage {
        id: uuid::Uuid::new_v4().to_string(),
        role: MessageRole::User,
        content: format!("{CARRYOVER_PREFIX}\n{summary}"),
        tool_data: None,
        timestamp: sealed.updated_at,
        tokens: estimate_tokens(&summary),
        external_id: None,
        attachments: None,
        trust_level: None,
        actor: None,
    };
    next.messages.insert(0, carryover);
    next.cached_message_count = None;
    next.set_rollover_pending(false);
    next.total_tokens = next.estimated_total_tokens() as u64;

    store.save(&next)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::provider::LlmProvider;
    use crate::llm::types::{
        ContentBlock, LlmMessage, LlmResponse, LlmToolDef, StopReason, TokenUsage,
    };
    use async_trait::async_trait;
    use workforce_common::session::InMemorySessionStore;

    /// LLM stub that always returns the same summary text.
    struct FixedLlm(&'static str);

    #[async_trait]
    impl LlmProvider for FixedLlm {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: self.0.to_string(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: "mock".to_string(),
            })
        }
    }

    /// LLM stub that always fails, for exercising seeding-failure recovery.
    struct FailingLlm;

    #[async_trait]
    impl LlmProvider for FailingLlm {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Err(eyre::eyre!("summary model unavailable"))
        }
    }

    /// Build a session with `pairs` user/assistant exchanges, each message
    /// sized to ~`tokens_each` tokens (estimate_tokens is chars/4).
    fn session_with(channel: &str, owner: &str, pairs: usize, tokens_each: u32) -> Session {
        let mut s = Session::new(channel, owner, "agent");
        let body = "x".repeat(tokens_each as usize * 4);
        for i in 0..pairs {
            s.add_user_message(&format!("u{i} {body}"), None);
            s.add_assistant_message(&format!("a{i} {body}"), tokens_each);
        }
        s
    }

    fn small_config() -> CompactionConfig {
        // trigger = 1000 * 0.5 = 500; watermark = 500 * 0.7 = 350.
        CompactionConfig {
            max_context_tokens: 1000,
            compaction_threshold: 0.5,
            rollover_warning_ratio: 0.7,
            min_messages_to_compact: 5,
            rollover_keep_recent: 4,
            ..Default::default()
        }
    }

    #[test]
    fn should_roll_false_below_watermark() {
        let s = session_with("ios", "u", 2, 10); // ~40 tokens, below watermark 350
        assert!(!should_roll(
            &s,
            &small_config(),
            ReservedContext::default(),
            10
        ));
    }

    #[test]
    fn should_roll_true_at_watermark_with_enough_messages() {
        let s = session_with("ios", "u", 10, 30); // ~600 tokens, above watermark 350
        assert!(should_roll(
            &s,
            &small_config(),
            ReservedContext::default(),
            6
        ));
    }

    #[test]
    fn should_roll_false_when_too_few_compactable() {
        let s = session_with("ios", "u", 10, 30); // above watermark
                                                  // Loop-guard: fewer than min_messages_to_compact (5) compactable.
        assert!(!should_roll(
            &s,
            &small_config(),
            ReservedContext::default(),
            2
        ));
    }

    #[test]
    fn keep_recent_is_larger_for_voice_sessions() {
        let cfg = small_config(); // rollover_keep_recent = 4
        let mut voice = Session::new("ios", "u", "agent");
        voice.metadata = serde_json::json!({ "voice_mode": true });
        let text = Session::new("slack", "u", "agent");
        assert_eq!(keep_recent_for(&voice, &cfg), VOICE_ROLLOVER_KEEP_RECENT);
        assert_eq!(keep_recent_for(&text, &cfg), cfg.rollover_keep_recent);
        assert!(VOICE_ROLLOVER_KEEP_RECENT > cfg.rollover_keep_recent);
    }

    #[test]
    fn crossed_watermark_keys_on_conversation_budget_not_reserved() {
        // trigger = 500; with reserved 400, budget = 100, watermark = 70.
        // A large reserved floor must NOT by itself trip the watermark.
        let cfg = CompactionConfig {
            max_context_tokens: 1000,
            compaction_threshold: 0.5,
            rollover_warning_ratio: 0.7,
            ..Default::default()
        };
        let reserved = ReservedContext {
            system_prompt_tokens: 400,
            tool_schema_tokens: 0,
            response_reservation_tokens: 0,
        };
        let small = session_with("ios", "u", 1, 20); // ~40 conversation tokens < 70
        assert!(!crossed_watermark(&small, &cfg, reserved));
        let big = session_with("ios", "u", 3, 20); // ~120 conversation tokens >= 70
        assert!(crossed_watermark(&big, &cfg, reserved));
    }

    #[test]
    fn should_roll_false_when_sealed_or_pending() {
        let mut s = session_with("ios", "u", 10, 30);
        s.set_rollover_pending(true);
        assert!(!should_roll(
            &s,
            &small_config(),
            ReservedContext::default(),
            6
        ));
        s.set_rollover_pending(false);
        s.seal();
        assert!(!should_roll(
            &s,
            &small_config(),
            ReservedContext::default(),
            6
        ));
    }

    #[test]
    fn seal_and_mint_seals_current_and_routes_to_successor() {
        let store = InMemorySessionStore::new();
        let mut current = store.get_or_create("ios", "u", "agent").unwrap();
        for i in 0..8 {
            current.add_user_message(&format!("u{i}"), None);
            current.add_assistant_message(&format!("a{i}"), 5);
        }
        store.save(&current).unwrap();

        let next = seal_and_mint(&store, &mut current, 4).unwrap();

        assert!(current.is_sealed());
        assert_eq!(next.seq(), current.seq() + 1);
        assert_eq!(next.prev_session_id(), Some(current.id.as_str()));
        assert!(next.rollover_pending());
        assert_eq!(next.messages.len(), 4, "carries last keep_recent verbatim");
        let carried: Vec<&str> = next.messages.iter().map(|m| m.content.as_str()).collect();
        let expected: Vec<&str> = current
            .recent_messages(4)
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(carried, expected);

        // The thread now routes to the unsealed successor.
        let active = store.get_or_create("ios", "u", "agent").unwrap();
        assert_eq!(active.id, next.id);
        assert!(store.get(&current.id).unwrap().unwrap().is_sealed());
    }

    #[test]
    fn seal_and_mint_carries_active_playbook_to_successor() {
        use workforce_common::playbook_state::{ArtifactRef, PlaybookProgress};

        let store = InMemorySessionStore::new();
        let mut current = store.get_or_create("ios", "u", "agent").unwrap();
        let mut progress =
            PlaybookProgress::new("design-impl-review", "design", "nonce1", "playbooks/s");
        progress.step_id = "implement".into();
        progress.step_model = Some("claude-opus-4-7".into());
        progress.artifacts.push(ArtifactRef {
            step_id: "design".into(),
            path: ".driftwerk/playbooks/s/design.md".into(),
            synopsis: "The design".into(),
        });
        current.set_playbook(&progress);
        store.save(&current).unwrap();

        let next = seal_and_mint(&store, &mut current, 4).unwrap();

        let carried = next.playbook().expect("playbook carried to successor");
        assert_eq!(carried, progress);
    }

    #[test]
    fn seal_and_mint_leaves_successor_playbook_absent_when_none() {
        let store = InMemorySessionStore::new();
        let mut current = store.get_or_create("ios", "u", "agent").unwrap();
        current.add_user_message("hi", None);
        store.save(&current).unwrap();

        let next = seal_and_mint(&store, &mut current, 4).unwrap();
        assert!(next.playbook().is_none());
    }

    #[test]
    fn get_or_create_mints_fresh_when_all_sealed() {
        let store = InMemorySessionStore::new();
        let mut first = store.get_or_create("ios", "u", "agent").unwrap();
        first.add_user_message("hi", None);
        store.save(&first).unwrap();
        first.seal();
        store.save(&first).unwrap();

        let fresh = store.get_or_create("ios", "u", "agent").unwrap();
        assert_ne!(fresh.id, first.id);
        assert!(!fresh.is_sealed());
    }

    #[tokio::test]
    async fn seed_summary_prepends_carryover_and_clears_pending() {
        let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
        let mut current = store.get_or_create("ios", "u", "agent").unwrap();
        for i in 0..8 {
            current.add_user_message(&format!("user message {i}"), None);
            current.add_assistant_message(&format!("assistant reply {i}"), 5);
        }
        store.save(&current).unwrap();
        let next = seal_and_mint(store.as_ref(), &mut current, 4).unwrap();

        let engine = CompactionEngine::new(
            small_config(),
            Arc::new(FixedLlm("Summary of the earlier conversation.")),
            Some("mock".to_string()),
        );
        seed_summary(
            Arc::clone(&store),
            engine,
            current.id.clone(),
            next.id.clone(),
            4,
        )
        .await
        .unwrap();

        let seeded = store.get(&next.id).unwrap().unwrap();
        assert!(!seeded.rollover_pending());
        assert!(
            seeded.messages[0].content.starts_with(CARRYOVER_PREFIX),
            "carry-over summary is prepended ahead of the verbatim turns"
        );
        assert_eq!(seeded.messages.len(), 5, "summary + 4 verbatim turns");
    }

    #[tokio::test]
    async fn seed_failure_clears_pending_so_thread_can_roll_again() {
        let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
        let mut current = store.get_or_create("ios", "u", "agent").unwrap();
        for i in 0..8 {
            current.add_user_message(&format!("user message {i}"), None);
            current.add_assistant_message(&format!("assistant reply {i}"), 5);
        }
        store.save(&current).unwrap();
        let next = seal_and_mint(store.as_ref(), &mut current, 4).unwrap();
        assert!(next.rollover_pending());

        let engine = CompactionEngine::new(
            small_config(),
            Arc::new(FailingLlm),
            Some("mock".to_string()),
        );
        seed_or_unwedge(
            Arc::clone(&store),
            engine,
            current.id.clone(),
            next.id.clone(),
            4,
        )
        .await;

        let after = store.get(&next.id).unwrap().unwrap();
        assert!(
            !after.rollover_pending(),
            "a failed seeding must not wedge the thread against future rollovers"
        );
        assert_eq!(after.messages.len(), 4, "verbatim turns intact, no summary");
    }

    fn voice_session(channel: &str, pairs: usize) -> Session {
        let mut s = session_with(channel, "u", pairs, 5);
        s.metadata = serde_json::json!({ "voice_mode": true, "tts_provider": "fishaudio" });
        s
    }

    fn at(rfc3339: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn backdate(session: &mut Session, to: chrono::DateTime<chrono::Utc>) {
        for m in &mut session.messages {
            m.timestamp = to;
        }
    }

    #[test]
    fn count_pressure_uses_voice_ceiling_for_voice_sessions() {
        let cfg = CompactionConfig {
            voice_rollover_max_messages: 10,
            chat_rollover_max_messages: 100,
            ..Default::default()
        };
        let voice = voice_session("ios", 5); // 10 conversational messages
        assert!(count_pressure(&voice, &cfg));
        let below = voice_session("ios", 4); // 8 messages
        assert!(!count_pressure(&below, &cfg));
    }

    #[test]
    fn count_pressure_uses_chat_ceiling_for_conversational_text() {
        let cfg = CompactionConfig {
            voice_rollover_max_messages: 10,
            chat_rollover_max_messages: 40,
            ..Default::default()
        };
        let text = session_with("ios", "u", 20, 5); // 40 messages, not voice
        assert!(count_pressure(&text, &cfg));
        let below = session_with("ios", "u", 10, 5); // 20 messages
        assert!(!count_pressure(&below, &cfg));
        // Slack-style channels have no count ceiling at all.
        let slack = session_with("slack:allan:D0X:171.1", "u", 200, 5);
        assert!(!count_pressure(&slack, &cfg));
    }

    #[test]
    fn count_pressure_disabled_by_zero() {
        let cfg = CompactionConfig {
            voice_rollover_max_messages: 0,
            chat_rollover_max_messages: 0,
            ..Default::default()
        };
        assert!(!count_pressure(&voice_session("ios", 100), &cfg));
        assert!(!count_pressure(&session_with("ios", "u", 100, 5), &cfg));
    }

    #[test]
    fn should_roll_fires_on_voice_count_below_watermark() {
        // Tiny tokens (~far below the watermark) but past the message ceiling:
        // exactly the prod failure mode a token-only trigger can't catch.
        let cfg = CompactionConfig {
            voice_rollover_max_messages: 10,
            min_messages_to_compact: 5,
            ..small_config()
        };
        let voice = voice_session("ios", 6); // 12 messages, ~60 tokens
        assert!(should_roll(&voice, &cfg, ReservedContext::default(), 6));
        // The compactable-count loop-guard still applies.
        assert!(!should_roll(&voice, &cfg, ReservedContext::default(), 2));
    }

    #[test]
    fn is_stale_keys_on_last_message_age() {
        let cfg = CompactionConfig::default(); // stale_rollover_hours = 24
        let now = at("2026-07-11T18:00:00Z");
        let mut fresh = voice_session("ios", 3);
        backdate(&mut fresh, at("2026-07-11T06:00:00Z")); // 12h ago
        assert!(!is_stale(&fresh, &cfg, now));
        let mut dormant = voice_session("ios", 3);
        backdate(&mut dormant, at("2026-05-25T18:00:00Z"));
        assert!(is_stale(&dormant, &cfg, now));
    }

    #[test]
    fn is_stale_false_for_empty_sealed_pending_or_disabled() {
        let cfg = CompactionConfig::default();
        let now = at("2026-07-11T18:00:00Z");
        let empty = Session::new("ios", "u", "agent");
        assert!(!is_stale(&empty, &cfg, now));

        let mut sealed = voice_session("ios", 3);
        backdate(&mut sealed, at("2026-05-25T18:00:00Z"));
        sealed.seal();
        assert!(!is_stale(&sealed, &cfg, now));

        let mut pending = voice_session("ios", 3);
        backdate(&mut pending, at("2026-05-25T18:00:00Z"));
        pending.set_rollover_pending(true);
        assert!(!is_stale(&pending, &cfg, now));

        let disabled = CompactionConfig {
            stale_rollover_hours: 0,
            ..Default::default()
        };
        let mut dormant = voice_session("ios", 3);
        backdate(&mut dormant, at("2026-05-25T18:00:00Z"));
        assert!(!is_stale(&dormant, &disabled, now));
    }

    #[test]
    fn roll_if_stale_passes_fresh_session_through_untouched() {
        let store = InMemorySessionStore::new();
        let mut session = store.get_or_create("ios", "u", "agent").unwrap();
        session.add_user_message("hi", None);
        store.save(&session).unwrap();
        let id = session.id.clone();

        let resumed = roll_if_stale(
            &store,
            session,
            &CompactionConfig::default(),
            chrono::Utc::now(),
        )
        .unwrap();

        assert_eq!(resumed.id, id);
        assert!(!store.get(&id).unwrap().unwrap().is_sealed());
    }

    #[test]
    fn roll_if_stale_seals_and_mints_dated_successor() {
        let store = InMemorySessionStore::new();
        let mut session = store.get_or_create("ios", "u", "agent").unwrap();
        session.metadata = serde_json::json!({ "voice_mode": true, "tts_provider": "fishaudio" });
        session.add_user_message("let's bump the pulumi image digest", None);
        session.add_assistant_message("bumping the deployment now", 5);
        backdate(&mut session, at("2026-05-25T18:00:00Z"));
        store.save(&session).unwrap();
        let old_id = session.id.clone();
        let old_seq = session.seq();

        let next = roll_if_stale(
            &store,
            session,
            &CompactionConfig::default(),
            at("2026-07-11T18:00:00Z"),
        )
        .unwrap();

        assert!(store.get(&old_id).unwrap().unwrap().is_sealed());
        assert_eq!(next.seq(), old_seq + 1);
        assert_eq!(next.prev_session_id(), Some(old_id.as_str()));
        assert!(!next.rollover_pending());
        assert!(next.is_voice(), "voice mode carried to successor");
        assert_eq!(
            next.metadata.get("tts_provider").and_then(|v| v.as_str()),
            Some("fishaudio")
        );

        assert_eq!(next.messages.len(), 1, "only the dated carry-over");
        let carryover = &next.messages[0].content;
        assert!(carryover.starts_with(CARRYOVER_PREFIX));
        assert!(carryover.contains("ended on 2026-05-25"), "{carryover}");
        assert!(carryover.contains("47 days ago"), "{carryover}");
        assert!(carryover.contains("historical context only"), "{carryover}");
        assert!(
            carryover.contains("pulumi image digest"),
            "final exchanges quoted: {carryover}"
        );
        assert!(
            carryover.contains("[2026-05-25] Assistant: bumping"),
            "{carryover}"
        );

        // The thread now routes to the fresh successor.
        let active = store.get_or_create("ios", "u", "agent").unwrap();
        assert_eq!(active.id, next.id);
    }

    /// A session holding `runs` runs of a job: its instruction, the one tool
    /// it calls and its answer.
    fn job_session(store: &InMemorySessionStore, runs: usize) -> Session {
        let mut session = store
            .get_or_create("scheduler:job:pinned:1", "scheduler", "agent")
            .unwrap();
        for run in 0..runs {
            session.add_user_message("check the queue", None);
            session.add_tool_message(
                "queue.depth",
                &serde_json::json!({}),
                &serde_json::json!({ "depth": run }),
                true,
                5,
                None,
            );
            session.add_assistant_message(&format!("depth is {run}"), 4);
        }
        store.save(&session).unwrap();
        session
    }

    #[test]
    fn a_session_past_the_message_ceiling_rolls_onto_a_carry_over_of_its_last_runs() {
        let store = InMemorySessionStore::new();
        let session = job_session(&store, 10);
        let old_id = session.id.clone();

        let next = roll_if_over_ceiling(&store, session, 30, chrono::Utc::now()).unwrap();

        assert!(store.get(&old_id).unwrap().unwrap().is_sealed());
        assert_eq!(next.prev_session_id(), Some(old_id.as_str()));
        assert!(!next.rollover_pending());
        assert_eq!(next.messages.len(), 1, "only the carry-over");
        let carryover = &next.messages[0].content;
        assert!(carryover.starts_with(CARRYOVER_PREFIX));
        assert!(carryover.contains("30 earlier messages"), "{carryover}");
        assert!(carryover.contains("depth is 9"), "{carryover}");
        assert!(!carryover.contains("depth is 6"), "{carryover}");
        let active = store
            .get_or_create("scheduler:job:pinned:1", "scheduler", "agent")
            .unwrap();
        assert_eq!(active.id, next.id);
    }

    #[test]
    fn a_session_under_the_ceiling_or_with_none_set_is_left_alone() {
        let store = InMemorySessionStore::new();
        let session = job_session(&store, 10);
        let id = session.id.clone();

        let under = roll_if_over_ceiling(&store, session, 31, chrono::Utc::now()).unwrap();
        assert_eq!(under.id, id);
        let disabled = roll_if_over_ceiling(&store, under, 0, chrono::Utc::now()).unwrap();
        assert_eq!(disabled.id, id);
        assert!(!store.get(&id).unwrap().unwrap().is_sealed());
    }

    #[test]
    fn stale_successor_is_not_itself_stale_and_never_rerolls() {
        let store = InMemorySessionStore::new();
        let mut session = store.get_or_create("ios", "u", "agent").unwrap();
        session.add_user_message("old talk", None);
        backdate(&mut session, at("2026-05-25T18:00:00Z"));
        store.save(&session).unwrap();
        let cfg = CompactionConfig::default();
        let now = at("2026-07-11T18:00:00Z");

        let next = roll_if_stale(&store, session, &cfg, now).unwrap();
        let next_id = next.id.clone();
        assert!(
            !is_stale(&next, &cfg, now),
            "a carry-over-only successor must read as fresh"
        );

        // Re-resolving in the same instant (gateway anchor + turn path both
        // roll) and reconnecting weeks later must both return the SAME
        // successor — no session accretion until real dialogue resumes and
        // then itself goes dormant.
        let again = roll_if_stale(&store, next, &cfg, now).unwrap();
        assert_eq!(again.id, next_id);
        let much_later = roll_if_stale(&store, again, &cfg, at("2026-09-01T00:00:00Z")).unwrap();
        assert_eq!(much_later.id, next_id);

        // Metadata writes (voice-mode flags) alone never age a session into
        // staleness either — only genuine dialogue timestamps count.
        let mut flagged = store.get(&next_id).unwrap().unwrap();
        flagged.set_active_environment("prod");
        assert!(!is_stale(&flagged, &cfg, at("2026-09-01T00:00:00Z")));
    }

    #[test]
    fn second_generation_carryover_quotes_real_dialogue_not_inherited_carryover() {
        let store = InMemorySessionStore::new();
        let mut session = store.get_or_create("ios", "u", "agent").unwrap();
        session.add_user_message("may talk about pulumi", None);
        backdate(&mut session, at("2026-05-25T18:00:00Z"));
        store.save(&session).unwrap();
        let cfg = CompactionConfig::default();

        let mut gen1 = roll_if_stale(&store, session, &cfg, at("2026-07-11T18:00:00Z")).unwrap();

        gen1.add_user_message("july talk about the answer grader", None);
        gen1.add_assistant_message("the grader is live", 5);
        for m in gen1.messages.iter_mut().skip(1) {
            m.timestamp = at("2026-07-11T18:30:00Z");
        }
        store.save(&gen1).unwrap();

        let gen2 = roll_if_stale(&store, gen1, &cfg, at("2026-08-20T18:00:00Z")).unwrap();

        let carry = &gen2.messages[0].content;
        assert!(carry.contains("ended on 2026-07-11"), "{carry}");
        assert!(carry.contains("answer grader"), "{carry}");
        assert_eq!(
            carry.matches(CARRYOVER_PREFIX).count(),
            1,
            "carry-overs must never nest: {carry}"
        );
        assert!(
            !carry.contains("pulumi"),
            "inherited carry-over content must not leak forward: {carry}"
        );
    }

    #[test]
    fn stale_carryover_clips_long_messages_and_bounds_count() {
        let mut session = Session::new("ios", "u", "agent");
        for i in 0..10 {
            session.add_user_message(&format!("m{i} {}", "y".repeat(1000)), None);
        }
        backdate(&mut session, at("2026-07-01T00:00:00Z"));

        let content = stale_carryover_content(&session, at("2026-07-11T00:00:00Z"));

        assert!(content.contains('…'), "long messages are clipped");
        assert!(!content.contains(&"y".repeat(500)));
        assert!(!content.contains("m3 "), "only the final exchanges appear");
        assert!(content.contains("m9 "));
        assert!(content.contains("10 days ago"));
    }

    #[test]
    fn seal_and_mint_carries_conversation_mode_to_successor() {
        let store = InMemorySessionStore::new();
        let mut current = store.get_or_create("ios", "u", "agent").unwrap();
        current.metadata = serde_json::json!({ "voice_mode": true, "tts_provider": "fishaudio" });
        for i in 0..8 {
            current.add_user_message(&format!("u{i}"), None);
            current.add_assistant_message(&format!("a{i}"), 5);
        }
        store.save(&current).unwrap();

        let next = seal_and_mint(&store, &mut current, 4).unwrap();

        assert!(next.is_voice());
        assert_eq!(
            next.metadata.get("tts_provider").and_then(|v| v.as_str()),
            Some("fishaudio")
        );
        assert!(next.rollover_pending(), "watermark path still seeds async");
    }
}

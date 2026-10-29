//! Session compaction and context window management.
//!
//! When sessions grow long (many tool calls, large outputs), they can exceed
//! the LLM's context window. This module provides strategies to keep sessions
//! within limits:
//!
//! 1. **Pruning** — lightweight: truncate large tool outputs in-place.
//! 2. **Summarisation** — heavier: use the LLM to summarise old messages into
//!    a single condensed message, replacing the originals.
//! 3. **Memory flush** — before summarisation, ask the LLM to persist any
//!    important context to memory files so nothing is lost.

use std::sync::Arc;

use eyre::Result;
use tracing::{info, warn};

use workforce_common::cost::{CostEntry, CostTracker, TokenCounts};
use workforce_common::session::{
    estimate_tokens, CompactionConfig, CompactionEvent, CompactionKind, MessageRole,
    ReservedContext, Session,
};
use workforce_common::subscription::Origin;

use crate::llm::provider::LlmProvider;
use crate::llm::types::{ContentBlock, LlmMessage, LlmResponse};
use crate::runner::with_served_log;

/// Trait for persisting memory flush output to durable storage.
///
/// Implemented by the hub crate using `MemoryManager` to write flush output
/// to daily notes (human content) and compaction logs (technical content).
#[async_trait::async_trait]
pub trait MemoryWriter: Send + Sync {
    /// Persist human conversation content to daily notes (memory/YYYY-MM-DD.md).
    ///
    /// The `label` describes which compaction path triggered the flush
    /// (e.g. "compaction", "adaptive compaction", "emergency compaction").
    async fn write_flush(&self, content: &str, label: &str) -> Result<()>;

    /// Persist technical system logs to the compaction log directory
    /// (memory/compaction/YYYY-MM-DD-compact.md).
    ///
    /// This is for tool execution details, file paths, configurations,
    /// and other technical context that shouldn't clutter daily notes.
    /// Default implementation is a no-op for backward compatibility.
    async fn write_technical_log(&self, _content: &str, _label: &str) -> Result<()> {
        Ok(())
    }
}

/// Task-oriented summary prompt — the default for text/agentic sessions where
/// the priority is preserving tasks, artifacts, and decisions.
const DEFAULT_SUMMARY_PROMPT: &str = concat!(
    "You are summarising a conversation to preserve continuity. This summary REPLACES ",
    "the original messages — the assistant will ONLY see this summary, not the original.\n\n",
    "Write a DETAILED summary with these sections:\n\n",
    "## Active Tasks\n",
    "What the user is currently asking for, including exact requirements and corrections given.\n\n",
    "## Key Content\n",
    "Any drafts, code, data, or artifacts produced. Include the ACTUAL content where possible, ",
    "not just 'a draft was created'.\n\n",
    "## Decisions & Corrections\n",
    "What was decided, what the user corrected or rejected, and why.\n\n",
    "## Context\n",
    "Background information the assistant needs to continue naturally.\n\n",
    "Be thorough. Include specific details, names, and content. Aim for at least 500 words. ",
    "Do NOT be brief — a short summary will cause the assistant to lose context and fail."
);

/// Conversation-oriented summary prompt for voice sessions. A spoken chat is a
/// flowing personal dialogue, not a task list; this preserves the thread of
/// discussion, the user's views and mood, and open threads so the assistant
/// picks up naturally instead of "forgetting" what was being talked about.
const VOICE_SUMMARY_PROMPT: &str = concat!(
    "You are summarising a VOICE conversation to preserve continuity. This summary REPLACES ",
    "the original messages — you will ONLY see this summary, not the original transcript.\n\n",
    "This is a flowing, personal spoken conversation, not a task list. Preserve the SUBSTANCE ",
    "of the dialogue so you can carry on as if you remember it:\n\n",
    "## Conversation So Far\n",
    "The topics discussed and how they connected — the actual thread of what you have been ",
    "talking about and exploring together. Capture the reasoning, anecdotes, analogies, ",
    "opinions, and the emotional tone, not just conclusions.\n\n",
    "## What Matters to the User\n",
    "Their views, feelings, decisions, and anything personal they shared (people, plans, ",
    "context about their life or work).\n\n",
    "## Open Threads\n",
    "Questions or topics left mid-discussion that you might naturally return to.\n\n",
    "## Concrete Items\n",
    "Any tasks, facts, or commitments that came up, if any.\n\n",
    "Write so you can resume the conversation naturally. Be thorough and specific — names, ",
    "details, the actual content of what was said. Aim for at least 500 words. A thin summary ",
    "will make you seem like you forgot the whole conversation."
);

/// The compaction engine — orchestrates pruning, memory flush, and summarisation.
pub struct CompactionEngine {
    config: CompactionConfig,
    llm: Arc<dyn LlmProvider>,
    model: Option<String>,
    memory_writer: Option<Arc<dyn MemoryWriter>>,
    cost_tracker: Option<Arc<CostTracker>>,
    agent_id: Option<String>,
}

/// The ambient identity for cost attribution, when a user is in scope.
fn current_identity() -> Option<String> {
    crate::runner::CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty())
}

impl CompactionEngine {
    /// Create a new compaction engine.
    pub fn new(config: CompactionConfig, llm: Arc<dyn LlmProvider>, model: Option<String>) -> Self {
        let effective_model = config.compaction_model.clone().or(model);
        Self::warn_if_expensive_model(&effective_model);
        Self {
            config,
            llm,
            model: effective_model,
            memory_writer: None,
            cost_tracker: None,
            agent_id: None,
        }
    }

    /// Create a compaction engine with a memory writer for persisting flush output.
    pub fn with_memory_writer(
        config: CompactionConfig,
        llm: Arc<dyn LlmProvider>,
        model: Option<String>,
        memory_writer: Arc<dyn MemoryWriter>,
    ) -> Self {
        let effective_model = config.compaction_model.clone().or(model);
        Self::warn_if_expensive_model(&effective_model);
        Self {
            config,
            llm,
            model: effective_model,
            memory_writer: Some(memory_writer),
            cost_tracker: None,
            agent_id: None,
        }
    }

    /// Attach a cost tracker so compaction LLM calls are recorded.
    pub fn with_cost_tracker(mut self, cost_tracker: Arc<CostTracker>, agent_id: String) -> Self {
        self.cost_tracker = Some(cost_tracker);
        self.agent_id = Some(agent_id);
        self
    }

    /// Log a warning when compaction would use an expensive model.
    fn warn_if_expensive_model(model: &Option<String>) {
        if let Some(m) = model.as_deref() {
            if m.contains("opus") {
                warn!(
                    model = m,
                    "Compaction using expensive model. Set compaction_model in \
                     [compaction] config to 'cheap' or a specific model to reduce costs."
                );
            }
        }
    }

    /// Inject a brief orientation message after compaction so the model knows
    /// its context was compressed and doesn't get disoriented.
    fn inject_orientation(session: &mut Session, event: &CompactionEvent) {
        let orientation = format!(
            "[System note: context was compacted. {} messages were summarised into a condensed \
             summary above. Tokens reduced from ~{} to ~{}. The conversation summary preserves \
             all key context. Continue naturally from where you left off.]",
            event.messages_compacted, event.tokens_before, event.tokens_after,
        );
        session.add_user_message(&orientation, None);
    }

    /// Check whether the session needs compaction and perform it if so.
    ///
    /// This is the main entry point — call it before sending messages to the LLM.
    /// It applies strategies in order:
    /// 1. Prune large tool outputs (if enabled)
    /// 2. If still over threshold, multi-pass summarisation compaction
    ///
    /// Returns `Ok(true)` if compaction was performed, `Ok(false)` if not needed.
    pub async fn compact_if_needed(
        &self,
        session: &mut Session,
        reserved: ReservedContext,
    ) -> Result<bool> {
        if !session.needs_compaction(&self.config, reserved) {
            return Ok(false);
        }

        info!(
            session_id = %session.id,
            estimated_tokens = session.estimated_total_tokens(),
            threshold = self.config.trigger_threshold(),
            "Session exceeds compaction threshold"
        );

        // Step 1: Try pruning first (lightweight)
        if self.prune(session, "Pruned tool outputs")
            && !session.needs_compaction(&self.config, reserved)
        {
            return Ok(true);
        }

        // Step 2: Multi-pass summarisation compaction
        let max_passes = self.config.max_compaction_passes;
        for pass in 0..max_passes {
            let tokens_before = session.estimated_total_tokens();

            // Check minimum messages threshold — avoid degenerate summaries
            let compactable = self.messages_to_compact(session);
            if compactable.len() < self.config.min_messages_to_compact {
                // Try adaptive keep_recent: reduce the keep window so normal
                // compaction can proceed instead of falling through to emergency.
                let total_compactable = session
                    .messages
                    .iter()
                    .filter(|m| m.role != MessageRole::System || m.is_compaction_summary())
                    .count();
                let needed = total_compactable.saturating_sub(self.config.min_messages_to_compact);
                let adaptive_keep = needed.max(3); // never fewer than 3 recent

                drop(compactable);

                let retry = self.messages_to_compact_with_keep(session, adaptive_keep);
                if adaptive_keep < self.config.keep_recent_messages
                    && retry.len() >= self.config.min_messages_to_compact
                {
                    info!(
                        session_id = %session.id,
                        original_keep = self.config.keep_recent_messages,
                        adaptive_keep,
                        compactable_messages = retry.len(),
                        "Using adaptive keep_recent for normal compaction"
                    );
                    drop(retry);

                    let summary = if self.config.enable_memory_flush {
                        let (flush_result, summary_result) = tokio::join!(
                            self.memory_flush_with_keep(session, adaptive_keep),
                            self.summarise_old_messages_with_keep(session, adaptive_keep),
                        );
                        self.persist_flush_result(session, flush_result, "adaptive compaction")
                            .await;
                        summary_result?
                    } else {
                        self.summarise_old_messages_with_keep(session, adaptive_keep)
                            .await?
                    };
                    let event = session.compact_with_summary(
                        &summary,
                        adaptive_keep,
                        CompactionKind::Summarisation,
                    );

                    let tokens_after = session.estimated_total_tokens();
                    info!(
                        session_id = %session.id,
                        pass = pass + 1,
                        tokens_before = event.tokens_before,
                        tokens_after = event.tokens_after,
                        adaptive_keep,
                        "Adaptive compaction pass complete"
                    );

                    if tokens_after >= tokens_before {
                        warn!(
                            session_id = %session.id,
                            pass = pass + 1,
                            "Adaptive compaction made no progress, stopping"
                        );
                        break;
                    }
                    if !session.needs_compaction(&self.config, reserved) {
                        break;
                    }
                    continue;
                }
                drop(retry);

                info!(
                    session_id = %session.id,
                    compactable_messages = self.messages_to_compact(session).len(),
                    min_required = self.config.min_messages_to_compact,
                    "Too few messages to compact, skipping"
                );
                break;
            }
            drop(compactable);

            self.compact_full(session).await?;

            let tokens_after = session.estimated_total_tokens();

            info!(
                session_id = %session.id,
                pass = pass + 1,
                tokens_before,
                tokens_after,
                "Compaction pass complete"
            );

            // Safety valve: no progress
            if tokens_after >= tokens_before {
                warn!(
                    session_id = %session.id,
                    pass = pass + 1,
                    "Compaction made no progress, stopping"
                );
                break;
            }

            // Under threshold? Done.
            if !session.needs_compaction(&self.config, reserved) {
                break;
            }
        }

        // Emergency: if still over threshold after all passes, aggressively prune
        if session.needs_compaction(&self.config, reserved) {
            warn!(
                session_id = %session.id,
                tokens = session.estimated_total_tokens(),
                threshold = self.config.trigger_threshold(),
                "Normal compaction insufficient — running emergency compaction"
            );

            // 1. Aggressively prune ALL tool outputs (including recent) with a tight limit
            let emergency_output_limit = self.config.prune_tool_output_max_chars.min(300);
            let emergency_input_limit = self.config.prune_tool_input_max_chars.min(256);
            session.prune_tool_outputs(emergency_output_limit, emergency_input_limit, 0);

            // 2. Re-compact with minimal keep_recent (3 messages)
            let emergency_keep = 3;
            let summary = if self.config.enable_memory_flush {
                let (flush_result, summary_result) = tokio::join!(
                    self.memory_flush_with_keep(session, emergency_keep),
                    self.summarise_old_messages_with_keep(session, emergency_keep),
                );
                self.persist_flush_result(session, flush_result, "emergency compaction")
                    .await;
                summary_result?
            } else {
                self.summarise_old_messages_with_keep(session, emergency_keep)
                    .await?
            };
            let event =
                session.compact_with_summary(&summary, emergency_keep, CompactionKind::Emergency);

            info!(
                session_id = %session.id,
                tokens_before = event.tokens_before,
                tokens_after = event.tokens_after,
                "Emergency compaction complete"
            );
        }

        Ok(true)
    }

    /// Perform only the pruning step of compaction (no LLM calls).
    ///
    /// This is a fast, lightweight operation (~1ms) that truncates large tool
    /// outputs. Used in the "Yellow zone" to free some tokens before deferring
    /// full compaction to a background task.
    pub fn compact_pruning_only(&self, session: &mut Session) -> Result<bool> {
        Ok(self.prune(session, "Pruned tool outputs (deferred compaction)"))
    }

    /// Truncate the large tool outputs older than the recent window and
    /// record the pruning. Whether any were truncated.
    fn prune(&self, session: &mut Session, logged: &'static str) -> bool {
        if !self.config.prune_tool_outputs {
            return false;
        }
        let pruned = session.prune_tool_outputs(
            self.config.prune_tool_output_max_chars,
            self.config.prune_tool_input_max_chars,
            self.config.keep_recent_messages,
        );
        if pruned == 0 {
            return false;
        }
        info!(
            session_id = %session.id,
            pruned_messages = pruned,
            tokens_after_prune = session.estimated_total_tokens(),
            "{logged}"
        );
        session.record_compaction(CompactionEvent {
            timestamp: chrono::Utc::now(),
            messages_compacted: pruned,
            tokens_before: session.total_tokens,
            tokens_after: session.estimated_total_tokens() as u64,
            kind: CompactionKind::Pruning,
        });
        true
    }

    /// Perform a full summarisation compaction.
    ///
    /// 1. (Optional) Memory flush — ask the LLM to persist important context
    /// 2. Summarise old messages using the LLM
    /// 3. Replace old messages with the summary
    pub async fn compact_full(&self, session: &mut Session) -> Result<CompactionEvent> {
        info!(
            session_id = %session.id,
            message_count = session.message_count(),
            estimated_tokens = session.estimated_total_tokens(),
            "Starting full compaction"
        );

        // Run memory flush and summarisation in parallel when both are needed.
        // Both read &Session immutably so this is safe.
        let summary = if self.config.enable_memory_flush {
            let (flush_result, summary_result) = tokio::join!(
                self.memory_flush(session),
                self.summarise_old_messages(session),
            );

            self.persist_flush_result(session, flush_result, "compaction")
                .await;

            summary_result?
        } else {
            self.summarise_old_messages(session).await?
        };

        // Step 3: Replace old messages with the summary
        let event = session.compact_with_summary(
            &summary,
            self.config.keep_recent_messages,
            CompactionKind::Summarisation,
        );

        // Step 4: Inject orientation message so the model knows compaction occurred
        Self::inject_orientation(session, &event);

        info!(
            session_id = %session.id,
            messages_compacted = event.messages_compacted,
            tokens_before = event.tokens_before,
            tokens_after = event.tokens_after,
            "Compaction complete"
        );

        Ok(event)
    }

    /// Perform a manual compaction (triggered by user command).
    pub async fn compact_manual(&self, session: &mut Session) -> Result<CompactionEvent> {
        info!(
            session_id = %session.id,
            "Manual compaction triggered"
        );

        // Memory flush first
        if self.config.enable_memory_flush {
            let flush_result = self.memory_flush(session).await;
            self.persist_flush_result(session, flush_result, "manual compaction")
                .await;
        }

        let summary = self.summarise_old_messages(session).await?;
        let event = session.compact_with_summary(
            &summary,
            self.config.keep_recent_messages,
            CompactionKind::Manual,
        );

        Self::inject_orientation(session, &event);

        info!(
            session_id = %session.id,
            messages_compacted = event.messages_compacted,
            tokens_before = event.tokens_before,
            tokens_after = event.tokens_after,
            "Manual compaction complete"
        );

        Ok(event)
    }

    /// Ask the LLM to flush important context to memory.
    ///
    /// This is a separate, short LLM call that asks the model to identify
    /// any important information that should be persisted before old messages
    /// are discarded.
    async fn memory_flush(&self, session: &Session) -> Result<String> {
        self.memory_flush_with_keep(session, self.config.keep_recent_messages)
            .await
    }

    /// Like `memory_flush` but with a custom keep_recent count.
    ///
    /// Used by adaptive and emergency compaction paths where the keep window
    /// differs from the configured default.
    async fn memory_flush_with_keep(
        &self,
        session: &Session,
        keep_recent: usize,
    ) -> Result<String> {
        let messages_to_summarise = self.messages_to_compact_with_keep(session, keep_recent);
        if messages_to_summarise.is_empty() {
            return Ok(String::new());
        }

        let conversation_text = format_messages_for_summary(&messages_to_summarise);

        let default_flush_prompt = concat!(
            "You are a memory assistant. The conversation below is about to be compacted. ",
            "Your job is to extract the HUMAN conversation content worth remembering.\n\n",
            "Focus on:\n",
            "- What personal topics were discussed (life events, feelings, plans, opinions)\n",
            "- Decisions, commitments, or plans the human mentioned\n",
            "- Preferences, instructions, or feedback the human gave\n",
            "- Memorable moments or emotional context\n",
            "- Important project context or business decisions discussed\n\n",
            "Do NOT include:\n",
            "- Technical tool outputs, file paths, or configurations\n",
            "- System operations, compaction details, or error logs\n",
            "- Raw code snippets or command outputs\n\n",
            "Write as concise notes in first person from the agent's perspective, ",
            "e.g. 'Kings mentioned he was tired today. We discussed the LLM migration plan.'"
        );
        let system_prompt = self
            .config
            .flush_prompt
            .as_deref()
            .unwrap_or(default_flush_prompt);

        let user_msg = format!(
            "Here is the conversation to be compacted:\n\n{}",
            conversation_text
        );

        let response = self
            .recorded_call(
                system_prompt,
                user_msg,
                2048,
                "memory_flush",
                "compaction:memory_flush",
            )
            .await?;

        let text = extract_text_from_response(&response.content);

        info!(
            session_id = %session.id,
            memory_flush_tokens = response.usage.input_tokens + response.usage.output_tokens,
            keep_recent,
            "Memory flush complete"
        );

        Ok(text)
    }

    /// Persist the result of a memory flush call via the configured `MemoryWriter`.
    ///
    /// Shared helper used by `compact_full`, adaptive, emergency, and manual paths.
    /// The `label` is included in the daily-note heading for traceability.
    async fn persist_flush_result(
        &self,
        session: &Session,
        flush_result: Result<String>,
        label: &str,
    ) {
        match flush_result {
            Ok(text) if !text.is_empty() => {
                if let Some(ref writer) = self.memory_writer {
                    if let Err(e) = writer.write_flush(&text, label).await {
                        warn!(
                            session_id = %session.id,
                            error = %e,
                            label,
                            "Failed to persist memory flush output"
                        );
                    } else {
                        info!(
                            session_id = %session.id,
                            chars = text.len(),
                            label,
                            "Memory flush persisted to daily notes"
                        );
                    }
                } else {
                    warn!(
                        session_id = %session.id,
                        "Memory flush generated output but no writer configured — discarding"
                    );
                }
            }
            Ok(_) => {} // empty flush, nothing to persist
            Err(e) => {
                warn!(
                    session_id = %session.id,
                    error = %e,
                    label,
                    "Memory flush failed, continuing with compaction"
                );
            }
        }
    }

    /// Select the summarisation system prompt: an explicit per-agent
    /// `summary_prompt` override wins; otherwise voice sessions get the
    /// conversation-oriented prompt and everything else the task-oriented one.
    fn summary_system_prompt(&self, session: &Session) -> &str {
        if let Some(ref custom) = self.config.summary_prompt {
            custom
        } else if session.is_voice() {
            VOICE_SUMMARY_PROMPT
        } else {
            DEFAULT_SUMMARY_PROMPT
        }
    }

    /// Summarise the old messages (those that will be compacted) into a single text.
    async fn summarise_old_messages(&self, session: &Session) -> Result<String> {
        let messages_to_summarise = self.messages_to_compact(session);
        if messages_to_summarise.is_empty() {
            return Ok("No prior conversation to summarise.".to_string());
        }

        // Estimate tokens in messages being compacted
        let compacting_tokens: u32 = messages_to_summarise.iter().map(|m| m.tokens).sum();

        // Dynamic token budget: target ~20% of input, clamped to [1024, 16384].
        // This prevents the summary from being as large as (or larger than) the input
        // when compacting only a few messages.
        let target_summary_tokens = (compacting_tokens / 5).clamp(1024, 32768);

        // Filter out low-value (and, in a shared conversation, all) tool messages
        // before sending to the LLM.
        let filtered: Vec<&workforce_common::session::SessionMessage> = messages_to_summarise
            .into_iter()
            .filter(|msg| keep_message_for_summary(session, msg))
            .collect();

        if filtered.is_empty() {
            return Ok("No substantive conversation to summarise.".to_string());
        }

        let conversation_text = format_messages_for_summary(&filtered);

        let system_prompt = self.summary_system_prompt(session);

        let text = self
            .summarise_with_retry(
                system_prompt,
                &conversation_text,
                target_summary_tokens,
                compacting_tokens,
            )
            .await?;

        info!(
            session_id = %session.id,
            summary_tokens = estimate_tokens(&text),
            target_summary_tokens,
            compacting_tokens,
            "Summarisation complete"
        );

        Ok(text)
    }

    /// Determine which messages should be compacted (everything except
    /// non-summary system messages and the most recent N messages).
    fn messages_to_compact<'a>(
        &self,
        session: &'a Session,
    ) -> Vec<&'a workforce_common::session::SessionMessage> {
        self.messages_to_compact_with_keep(session, self.config.keep_recent_messages)
    }

    /// Like `messages_to_compact` but with a custom keep_recent count.
    fn messages_to_compact_with_keep<'a>(
        &self,
        session: &'a Session,
        keep_recent: usize,
    ) -> Vec<&'a workforce_common::session::SessionMessage> {
        let total = session.messages.len();
        let keep_from = total.saturating_sub(keep_recent);

        session.messages[..keep_from]
            .iter()
            .filter(|m| m.role != MessageRole::System || m.is_compaction_summary())
            .collect()
    }

    /// Number of messages compaction would summarise at the given keep-recent
    /// window. Rollover uses this to skip threads with too little compactable
    /// content (loop-guard against minting near-empty successors).
    pub fn compactable_message_count(&self, session: &Session, keep_recent: usize) -> usize {
        self.messages_to_compact_with_keep(session, keep_recent)
            .len()
    }

    /// Produce a carry-over summary of everything older than the last
    /// `keep_recent` turns, from an immutable session view — reusing the
    /// summarisation prompt, token budget, and retry of normal compaction
    /// without mutating the session. The rollover path seeds the result onto
    /// the successor session.
    pub async fn produce_carryover_summary(
        &self,
        session: &Session,
        keep_recent: usize,
    ) -> Result<String> {
        self.summarise_old_messages_with_keep(session, keep_recent)
            .await
    }

    /// Summarise old messages with a custom keep_recent count (for emergency compaction).
    async fn summarise_old_messages_with_keep(
        &self,
        session: &Session,
        keep_recent: usize,
    ) -> Result<String> {
        let messages_to_summarise = self.messages_to_compact_with_keep(session, keep_recent);
        if messages_to_summarise.is_empty() {
            return Ok("No prior conversation to summarise.".to_string());
        }

        let compacting_tokens: u32 = messages_to_summarise.iter().map(|m| m.tokens).sum();
        let target_summary_tokens = (compacting_tokens / 5).clamp(1024, 32768);

        let filtered: Vec<&workforce_common::session::SessionMessage> = messages_to_summarise
            .into_iter()
            .filter(|msg| keep_message_for_summary(session, msg))
            .collect();

        if filtered.is_empty() {
            return Ok("No substantive conversation to summarise.".to_string());
        }

        let conversation_text = format_messages_for_summary(&filtered);

        let system_prompt = self.summary_system_prompt(session);

        let text = self
            .summarise_with_retry(
                system_prompt,
                &conversation_text,
                target_summary_tokens,
                compacting_tokens,
            )
            .await?;

        info!(
            session_id = %session.id,
            summary_tokens = estimate_tokens(&text),
            target_summary_tokens,
            compacting_tokens,
            keep_recent,
            "Emergency summarisation complete"
        );

        Ok(text)
    }

    /// One compaction LLM call, recorded against the agent under its own
    /// served log.
    async fn recorded_call(
        &self,
        system_prompt: &str,
        user_msg: String,
        max_tokens: u32,
        task_type: &str,
        description: &str,
    ) -> Result<LlmResponse> {
        let (response, served) = with_served_log(self.llm.create_message(
            Some(system_prompt),
            &[LlmMessage::user(user_msg)],
            &[],
            self.model.as_deref(),
            Some(max_tokens),
            true,
        ))
        .await;
        let response = response?;
        if let Some(ref ct) = self.cost_tracker {
            let identity = current_identity();
            ct.record(CostEntry {
                task_type: Some(task_type),
                identity_id: identity.as_deref(),
                description: Some(description),
                served,
                ..CostEntry::new(
                    self.agent_id.as_deref().unwrap_or("compaction"),
                    self.model.as_deref().unwrap_or("unknown"),
                    TokenCounts::from(&response.usage),
                    Origin::Compaction,
                )
            });
        }
        Ok(response)
    }

    /// Call the LLM to summarise, retrying once if the output is absurdly short.
    async fn summarise_with_retry(
        &self,
        system_prompt: &str,
        conversation_text: &str,
        target_summary_tokens: u32,
        compacting_tokens: u32,
    ) -> Result<String> {
        let user_msg = format!("Summarise this conversation:\n\n{conversation_text}");

        let response = self
            .recorded_call(
                system_prompt,
                user_msg,
                target_summary_tokens,
                "compaction_summary",
                "compaction:summary",
            )
            .await?;

        let text = extract_text_from_response(&response.content);
        let summary_tokens = estimate_tokens(&text);

        let min_expected = (compacting_tokens / 20).max(256);
        if summary_tokens < min_expected && compacting_tokens > 5000 {
            warn!(
                summary_tokens,
                compacting_tokens,
                min_expected,
                "Summary too short, retrying with explicit length requirement"
            );

            let retry_msg = format!(
                "Your previous summary was only {} words — far too brief for a {} word conversation. \
                 Rewrite it with AT LEAST {} words. Include ALL specific details: names, content of \
                 drafts, exact user requests and corrections, and current task state.\n\n{}",
                summary_tokens,
                compacting_tokens,
                min_expected / 2,
                conversation_text
            );

            let retry_response = self
                .recorded_call(
                    system_prompt,
                    retry_msg,
                    target_summary_tokens,
                    "compaction_summary",
                    "compaction:summary_retry",
                )
                .await?;

            let retry_text = extract_text_from_response(&retry_response.content);
            let retry_tokens = estimate_tokens(&retry_text);

            info!(
                original_tokens = summary_tokens,
                retry_tokens, "Summary retry complete"
            );

            if retry_tokens > summary_tokens {
                return Ok(retry_text);
            }
        }

        Ok(text)
    }
}

/// Whether a to-be-compacted message should feed the summary. Non-tool messages
/// always do. A tool message is dropped when it carries little information
/// (already-truncated output or a trivial completion note) and — in a shared
/// multi-human conversation — unconditionally: the summary is replayed to every
/// participant via the rollover carry-over, so it must carry only the visible
/// layer (user + assistant) and never launder one user's raw tool output to
/// another. The current speaker's own recent tool output is preserved
/// un-summarised and actor-filtered at replay time.
fn keep_message_for_summary(
    session: &Session,
    msg: &workforce_common::session::SessionMessage,
) -> bool {
    if msg.role != MessageRole::Tool {
        return true;
    }
    if session.is_shared_conversation() {
        return false;
    }
    let headline = msg.headline();
    if headline.contains("[truncated") || headline.contains("[output truncated") {
        return false;
    }
    let status_only = msg.content.trim_end() == headline && headline.len() < 100;
    !(status_only && (headline.contains("completed in") || headline.contains("executed in")))
}

/// Format session messages into a readable text for LLM summarisation.
fn format_messages_for_summary(messages: &[&workforce_common::session::SessionMessage]) -> String {
    let mut parts = Vec::with_capacity(messages.len());

    for msg in messages {
        let role_label = match msg.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::System => "System",
            MessageRole::Tool => "Tool",
        };

        let content = if msg.content.len() > 1000 {
            let mut end = 1000;
            while !msg.content.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}... [truncated]", &msg.content[..end])
        } else {
            msg.content.clone()
        };

        let tool_info = msg
            .tool_data
            .as_ref()
            .and_then(|td| td.get("name"))
            .and_then(|n| n.as_str())
            .map(|name| format!(" [tool: {}]", name))
            .unwrap_or_default();

        parts.push(format!("[{}{}]: {}", role_label, tool_info, content));
    }

    parts.join("\n\n")
}

/// Extract text content from an LLM response, joining multiple text blocks.
pub fn extract_text_from_response(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{LlmResponse, LlmToolDef, StopReason, TokenUsage};
    use async_trait::async_trait;
    use workforce_common::session::Session;

    /// Mock LLM that returns configurable text responses.
    struct MockCompactionLlm {
        responses: std::sync::Mutex<Vec<String>>,
    }

    impl MockCompactionLlm {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: std::sync::Mutex::new(
                    responses.into_iter().map(String::from).rev().collect(),
                ),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockCompactionLlm {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let text = self
                .responses
                .lock()
                .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?
                .pop()
                .unwrap_or_else(|| "Default mock response".to_string());

            Ok(LlmResponse {
                content: vec![ContentBlock::Text { text }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Default::default()
                },
                model: "mock".to_string(),
            })
        }
    }

    /// Returns the user-role text it was handed, so a test can assert exactly
    /// which transcript content reached the summariser.
    struct EchoLlm;

    #[async_trait]
    impl LlmProvider for EchoLlm {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let text = messages
                .iter()
                .flat_map(|m| m.content.iter())
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(LlmResponse {
                content: vec![ContentBlock::Text { text }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Default::default()
                },
                model: "echo".to_string(),
            })
        }
    }

    fn make_large_session(message_count: usize) -> Session {
        let mut session = Session::new("test", "user", "agent");
        session.add_system_message("You are a test agent.");

        for i in 0..message_count {
            // Each message is ~100 chars = ~25 tokens
            let msg = format!(
                "This is test message number {} with enough content to take up some tokens in the session context window",
                i
            );
            session.add_user_message(&msg, None);
            session.add_assistant_message(&format!("Response to message {}", i), 25);
        }

        session
    }

    fn make_session_with_tool_outputs(tool_output_size: usize) -> Session {
        let mut session = Session::new("test", "user", "agent");
        session.add_system_message("You are a test agent.");

        for i in 0..10 {
            session.add_user_message(&format!("Do task {}", i), None);
            let large_output = "x".repeat(tool_output_size);
            session.add_tool_message(
                "exec.run",
                &serde_json::json!({"command": "ls"}),
                &serde_json::json!({"stdout": large_output}),
                true,
                100,
                None,
            );
            session.add_assistant_message(&format!("Done with task {}", i), 20);
        }

        session
    }

    #[test]
    fn summary_prompt_is_voice_aware() {
        let engine = CompactionEngine::new(
            CompactionConfig::default(),
            Arc::new(MockCompactionLlm::new(vec![])),
            None,
        );
        let mut voice = Session::new("ios", "u", "agent");
        voice.metadata = serde_json::json!({ "voice_mode": true });
        let text = Session::new("slack", "u", "agent");
        assert_eq!(engine.summary_system_prompt(&voice), VOICE_SUMMARY_PROMPT);
        assert_eq!(engine.summary_system_prompt(&text), DEFAULT_SUMMARY_PROMPT);

        // An explicit per-agent override wins over both defaults.
        let cfg = CompactionConfig {
            summary_prompt: Some("custom agent prompt".to_string()),
            ..Default::default()
        };
        let engine2 = CompactionEngine::new(cfg, Arc::new(MockCompactionLlm::new(vec![])), None);
        assert_eq!(engine2.summary_system_prompt(&voice), "custom agent prompt");
    }

    // ─── Compaction Trigger Detection ───

    #[test]
    fn needs_compaction_false_when_under_threshold() {
        let session = make_large_session(5);
        let config = CompactionConfig {
            max_context_tokens: 200_000,
            compaction_threshold: 0.8,
            ..Default::default()
        };
        assert!(!session.needs_compaction(&config, ReservedContext::default()));
    }

    #[test]
    fn needs_compaction_true_when_over_threshold() {
        let session = make_large_session(100);
        // Set a very low threshold so our session exceeds it
        let config = CompactionConfig {
            max_context_tokens: 100,
            compaction_threshold: 0.5,
            ..Default::default()
        };
        assert!(session.needs_compaction(&config, ReservedContext::default()));
    }

    #[test]
    fn needs_compaction_accounts_for_reserved_overhead() {
        let session = make_large_session(5);
        let config = CompactionConfig {
            max_context_tokens: 131_072,
            compaction_threshold: 0.5,
            ..Default::default()
        };
        let under = session.estimated_total_tokens();
        // Reserved overhead alone pushes the effective total past 50% of 131K.
        let reserved = ReservedContext {
            system_prompt_tokens: 60_000,
            tool_schema_tokens: 16_000,
            response_reservation_tokens: 0,
        };
        assert!(!session.needs_compaction(&config, ReservedContext::default()));
        assert!(under < config.trigger_threshold());
        assert!(session.needs_compaction(&config, reserved));
    }

    #[test]
    fn trigger_threshold_calculation() {
        let config = CompactionConfig {
            max_context_tokens: 200_000,
            compaction_threshold: 0.8,
            ..Default::default()
        };
        assert_eq!(config.trigger_threshold(), 160_000);

        let config2 = CompactionConfig {
            max_context_tokens: 100_000,
            compaction_threshold: 0.5,
            ..Default::default()
        };
        assert_eq!(config2.trigger_threshold(), 50_000);
    }

    // ─── Token Counting ───

    #[test]
    fn estimated_total_tokens_sums_all_messages() {
        let mut session = Session::new("test", "user", "agent");
        session.add_system_message("System prompt here"); // ~4 tokens
        session.add_user_message("Hello world", None); // ~3 tokens
        session.add_assistant_message("Hi there!", 10); // 10 tokens

        let estimated = session.estimated_total_tokens();
        assert!(estimated > 0);
        // System + user contribute via estimate_tokens, assistant is explicit 10
        assert!(estimated >= 10);
    }

    #[test]
    fn estimate_tokens_basic() {
        assert_eq!(estimate_tokens(""), 1); // min 1
        assert_eq!(estimate_tokens("hello"), 1); // 5 chars / 4 = 1
        assert_eq!(estimate_tokens("hello world test"), 4); // 16 chars / 4 = 4
        assert_eq!(estimate_tokens("a".repeat(400).as_str()), 100); // 400 / 4 = 100
    }

    // ─── Session Pruning ───

    #[test]
    fn prune_tool_outputs_truncates_large_outputs() {
        let mut session = make_session_with_tool_outputs(5000);
        let initial_count = session.message_count();

        let pruned = session.prune_tool_outputs(500, 1024, 6);
        assert!(pruned > 0, "Expected some messages to be pruned");

        // Message count should be unchanged (pruning modifies, doesn't remove)
        assert_eq!(session.message_count(), initial_count);

        // Check that old tool messages were truncated
        for msg in &session.messages[..session.messages.len().saturating_sub(6)] {
            if msg.role == MessageRole::Tool {
                if let Some(ref td) = msg.tool_data {
                    if let Some(output) = td.get("output") {
                        let output_str = output.to_string();
                        // Should be truncated (much less than 5000)
                        assert!(
                            output_str.len() < 1000,
                            "Tool output should be truncated, got {} chars",
                            output_str.len()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn prune_preserves_recent_messages() {
        let mut session = make_session_with_tool_outputs(5000);
        let recent_count = 6;
        let total = session.message_count();

        // Capture recent messages before pruning
        let recent_before: Vec<String> = session.messages[total - recent_count..]
            .iter()
            .map(|m| m.id.clone())
            .collect();

        session.prune_tool_outputs(500, 1024, recent_count);

        // Recent messages should be unchanged
        let recent_after: Vec<String> = session.messages[session.messages.len() - recent_count..]
            .iter()
            .map(|m| m.id.clone())
            .collect();

        assert_eq!(recent_before, recent_after);
    }

    #[test]
    fn prune_returns_zero_when_nothing_to_prune() {
        let mut session = Session::new("test", "user", "agent");
        session.add_user_message("Hello", None);
        session.add_assistant_message("Hi", 5);

        let pruned = session.prune_tool_outputs(2000, 1024, 10);
        assert_eq!(pruned, 0);
    }

    // ─── Full Compaction with Summary ───

    #[test]
    fn compact_with_summary_replaces_old_messages() {
        let mut session = make_large_session(50);
        let initial_count = session.message_count();

        let event = session.compact_with_summary(
            "Summary of the conversation so far.",
            10,
            CompactionKind::Summarisation,
        );

        // Should have fewer messages now
        assert!(session.message_count() < initial_count);

        // Should have the summary message
        let has_summary = session
            .messages
            .iter()
            .any(|m| m.content.contains("[Compaction Summary"));
        assert!(has_summary, "Expected compaction summary message");

        // Should keep 10 recent messages + system + summary
        assert!(session.message_count() <= 12); // system + summary + 10 recent

        // Compaction event metadata
        assert!(event.messages_compacted > 0);
        assert!(event.tokens_before > event.tokens_after);
        assert_eq!(event.kind, CompactionKind::Summarisation);
    }

    #[test]
    fn compact_preserves_non_summary_system_messages() {
        let mut session = make_large_session(50);
        let _event = session.compact_with_summary("Summary.", 10, CompactionKind::Summarisation);

        let system_msgs: Vec<_> = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::System)
            .collect();

        // Should have at least the original system message and the summary (which is also System)
        assert!(
            system_msgs.len() >= 2,
            "Expected at least 2 system messages (original + summary), got {}",
            system_msgs.len()
        );
    }

    /// A tool message whose content the existing low-value filter keeps (it is
    /// neither already-truncated nor a trivial completion note), so it isolates
    /// the shared-session exclusion from the pre-existing noise filter.
    fn substantive_tool_msg(name: &str) -> workforce_common::session::SessionMessage {
        workforce_common::session::SessionMessage {
            id: "t".to_string(),
            role: MessageRole::Tool,
            content: format!("Tool '{name}' returned 3 deploy records"),
            tool_data: Some(serde_json::json!({ "name": name, "output": "rows", "success": true })),
            timestamp: chrono::Utc::now(),
            tokens: 5,
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
            desk: None,
        }
    }

    #[test]
    fn keep_message_for_summary_excludes_all_tools_in_shared_session() {
        let shared = Session::new("c", "shared:t", "agent");
        let mut user = substantive_tool_msg("vault.get");
        user.role = MessageRole::User;
        assert!(keep_message_for_summary(&shared, &user));
        assert!(!keep_message_for_summary(
            &shared,
            &substantive_tool_msg("vault.get")
        ));
    }

    #[test]
    fn keep_message_for_summary_keeps_substantive_tools_in_private_session() {
        let private = Session::new("c", "U1", "agent");
        assert!(keep_message_for_summary(
            &private,
            &substantive_tool_msg("vault.get")
        ));
    }

    #[test]
    fn keep_message_for_summary_keeps_deliveries_and_drops_bare_completions() {
        use workforce_common::session::SessionMessage;
        let private = Session::new("c", "U1", "agent");
        let sent = |text: &str| {
            SessionMessage::tool(
                "messaging.send",
                &serde_json::json!({ "channel": "D1", "text": text }),
                &serde_json::json!({ "ok": true }),
                true,
                12,
                None,
            )
        };
        assert!(keep_message_for_summary(&private, &sent("Hi")));
        assert!(keep_message_for_summary(
            &private,
            &sent("the log said [truncated earlier] and executed in 3s")
        ));
        let bare = SessionMessage::tool(
            "shell",
            &serde_json::json!({ "command": "ls" }),
            &serde_json::json!("ok"),
            true,
            12,
            None,
        );
        assert!(!keep_message_for_summary(&private, &bare));
    }

    #[tokio::test]
    async fn shared_session_carryover_summary_excludes_tool_activity() {
        // The summary carries each tool's note + name, never its raw output. In a
        // shared session even that cross-user activity is excluded; the visible
        // layer (user + assistant) always survives.
        let build = |owner: &str| {
            let mut s = Session::new("c", owner, "agent");
            s.add_user_message("what is the deploy status?", None);
            s.messages.push(substantive_tool_msg("datadog.logs"));
            s.add_assistant_message("the deploy is green", 5);
            s
        };
        let engine = CompactionEngine::new(CompactionConfig::default(), Arc::new(EchoLlm), None);

        let shared = engine
            .produce_carryover_summary(&build("shared:thread-1"), 0)
            .await
            .unwrap();
        assert!(
            shared.contains("the deploy is green"),
            "shared carryover dropped the visible layer: {shared}"
        );
        assert!(
            !shared.contains("datadog.logs"),
            "shared carryover leaked cross-user tool activity: {shared}"
        );

        let private = engine
            .produce_carryover_summary(&build("U1"), 0)
            .await
            .unwrap();
        assert!(
            private.contains("datadog.logs"),
            "private carryover unexpectedly dropped tool activity"
        );
    }

    #[test]
    fn compact_folds_previous_compaction_summaries() {
        let mut session = make_large_session(50);

        // First compaction: creates a summary system message
        session.compact_with_summary(
            "First summary of the conversation.",
            10,
            CompactionKind::Summarisation,
        );

        let summaries_after_first: Vec<_> = session
            .messages
            .iter()
            .filter(|m| m.is_compaction_summary())
            .collect();
        assert_eq!(
            summaries_after_first.len(),
            1,
            "Expected 1 compaction summary after first compaction"
        );

        // Add more messages so there's something to compact again
        for i in 0..20 {
            session.add_user_message(&format!("New message {}", i), None);
            session.add_assistant_message(&format!("Response {}", i), 25);
        }

        // Second compaction: should fold the first summary into the new one
        session.compact_with_summary(
            "Second summary incorporating all prior context.",
            10,
            CompactionKind::Summarisation,
        );

        let summaries_after_second: Vec<_> = session
            .messages
            .iter()
            .filter(|m| m.is_compaction_summary())
            .collect();
        // Should still be exactly 1 summary — the old one was replaced, not accumulated
        assert_eq!(
            summaries_after_second.len(),
            1,
            "Expected 1 compaction summary after second compaction (old should be folded), got {}",
            summaries_after_second.len()
        );

        // The original system message ("You are a test agent.") should still be preserved
        let original_system = session
            .messages
            .iter()
            .any(|m| m.role == MessageRole::System && m.content.contains("test agent"));
        assert!(
            original_system,
            "Original system message should be preserved"
        );
    }

    // ─── Compaction Metadata Recording ───

    #[test]
    fn compaction_event_recorded_in_history() {
        let mut session = make_large_session(50);
        assert!(session.compaction_history.is_empty());

        session.compact_with_summary("Summary.", 10, CompactionKind::Summarisation);

        assert_eq!(session.compaction_history.len(), 1);
        let event = &session.compaction_history[0];
        assert_eq!(event.kind, CompactionKind::Summarisation);
        assert!(event.messages_compacted > 0);
    }

    #[test]
    fn multiple_compactions_accumulate_history() {
        let mut session = make_large_session(50);

        let _event =
            session.compact_with_summary("First summary.", 10, CompactionKind::Summarisation);
        session.record_compaction(CompactionEvent {
            timestamp: chrono::Utc::now(),
            messages_compacted: 5,
            tokens_before: 1000,
            tokens_after: 500,
            kind: CompactionKind::Pruning,
        });

        assert_eq!(session.compaction_history.len(), 2);
        assert_eq!(
            session.compaction_history[0].kind,
            CompactionKind::Summarisation
        );
        assert_eq!(session.compaction_history[1].kind, CompactionKind::Pruning);
    }

    // ─── LLM-Based Compaction (with Mock) ───

    #[tokio::test]
    async fn compact_if_needed_does_nothing_under_threshold() {
        let llm = Arc::new(MockCompactionLlm::new(vec![]));
        let config = CompactionConfig {
            max_context_tokens: 200_000,
            compaction_threshold: 0.8,
            ..Default::default()
        };
        let engine = CompactionEngine::new(config, llm, None);

        let mut session = make_large_session(5);
        let result = engine
            .compact_if_needed(&mut session, ReservedContext::default())
            .await;

        assert!(result.is_ok());
        assert!(!result.as_ref().ok().copied().unwrap_or(true));
        assert!(session.compaction_history.is_empty());
    }

    #[tokio::test]
    async fn compact_if_needed_prunes_then_summarises() {
        // Two responses: first for memory flush, second for summarisation
        let llm = Arc::new(MockCompactionLlm::new(vec![
            "Key context to remember: task was about testing.",
            "Summary: The user tested the compaction system with multiple messages.",
        ]));
        let config = CompactionConfig {
            max_context_tokens: 50, // Very low to force compaction
            compaction_threshold: 0.5,
            keep_recent_messages: 4,
            enable_memory_flush: true,
            prune_tool_outputs: true,
            prune_tool_output_max_chars: 100,
            ..Default::default()
        };
        let engine = CompactionEngine::new(config, llm, None);

        let mut session = make_large_session(20);
        let initial_count = session.message_count();

        let result = engine
            .compact_if_needed(&mut session, ReservedContext::default())
            .await;
        assert!(result.is_ok());
        assert!(result.ok().unwrap_or(false));

        // Session should be smaller
        assert!(session.message_count() < initial_count);

        // Should have compaction history
        assert!(!session.compaction_history.is_empty());
    }

    /// `runs` small exchanges that each call one tool with a large result:
    /// the stored-message estimate counts almost none of what they replay.
    fn tool_heavy_session(runs: usize) -> Session {
        let mut session = Session::new("test", "user", "agent");
        session.add_system_message("You are a test agent.");
        for run in 0..runs {
            session.add_user_message("check the queue", None);
            session.add_tool_message(
                "queue.dump",
                &serde_json::json!({}),
                &serde_json::json!("row ".repeat(2_000)),
                true,
                5,
                None,
            );
            session.add_assistant_message(&format!("depth is {run}"), 4);
        }
        session
    }

    fn kinds(session: &Session) -> Vec<CompactionKind> {
        session
            .compaction_history
            .iter()
            .map(|event| event.kind.clone())
            .collect()
    }

    fn production_sized() -> CompactionConfig {
        CompactionConfig {
            max_context_tokens: 200_000,
            compaction_threshold: 0.8,
            keep_recent_messages: 4,
            enable_memory_flush: false,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_tool_heavy_session_the_provider_measured_over_the_threshold_is_summarised() {
        let llm = Arc::new(MockCompactionLlm::new(vec!["Summary: the queue drained."]));
        let config = production_sized();
        let engine = CompactionEngine::new(config.clone(), llm, None);
        let reserved = ReservedContext::default();
        let mut session = tool_heavy_session(40);
        session.record_observed_input_tokens(180_000);
        assert!(
            session.estimated_total_with_reserved(reserved) < 2_000,
            "the stored-message estimate alone calls it small"
        );
        let before = session.message_count();

        let compacted = engine
            .compact_if_needed(&mut session, reserved)
            .await
            .unwrap();

        assert!(compacted);
        assert_eq!(
            kinds(&session),
            [CompactionKind::Pruning, CompactionKind::Summarisation],
            "pruning stored tool rows does not make the measured prompt small"
        );
        assert!(session.message_count() < before);
        assert!(!session.needs_compaction(&config, reserved));
    }

    #[tokio::test]
    async fn a_session_the_provider_measured_under_the_threshold_is_left_as_it_is() {
        let llm = Arc::new(MockCompactionLlm::new(vec![]));
        let engine = CompactionEngine::new(production_sized(), llm, None);
        for mut session in [tool_heavy_session(40), make_large_session(40)] {
            session.record_observed_input_tokens(150_000);
            let before = serde_json::to_value(&session).unwrap();

            let compacted = engine
                .compact_if_needed(&mut session, ReservedContext::default())
                .await
                .unwrap();

            assert!(!compacted);
            assert_eq!(serde_json::to_value(&session).unwrap(), before);
        }
    }

    #[test]
    fn a_deferred_pruning_leaves_the_session_due_for_the_compaction_it_deferred() {
        let llm = Arc::new(MockCompactionLlm::new(vec![]));
        let config = production_sized();
        let engine = CompactionEngine::new(config.clone(), llm, None);
        let mut session = tool_heavy_session(40);
        session.record_observed_input_tokens(180_000);

        assert!(engine.compact_pruning_only(&mut session).unwrap());

        assert_eq!(kinds(&session), [CompactionKind::Pruning]);
        assert!(session.needs_compaction(&config, ReservedContext::default()));
    }

    #[tokio::test]
    async fn compact_manual_always_runs() {
        let llm = Arc::new(MockCompactionLlm::new(vec![
            "Memory: important context.",
            "Manual summary of the conversation.",
        ]));
        let config = CompactionConfig {
            max_context_tokens: 200_000,
            compaction_threshold: 0.8,
            keep_recent_messages: 4,
            enable_memory_flush: true,
            ..Default::default()
        };
        let engine = CompactionEngine::new(config, llm, None);

        let mut session = make_large_session(20);
        let initial_count = session.message_count();

        let event = engine.compact_manual(&mut session).await;
        assert!(event.is_ok());

        let event = event.expect("compaction should succeed");
        assert_eq!(event.kind, CompactionKind::Manual);
        assert!(session.message_count() < initial_count);
    }

    #[tokio::test]
    async fn compact_full_with_memory_flush_disabled() {
        // Only one response needed (summarisation only, no memory flush)
        let llm = Arc::new(MockCompactionLlm::new(vec!["Summary of the conversation."]));
        let config = CompactionConfig {
            max_context_tokens: 50,
            compaction_threshold: 0.5,
            keep_recent_messages: 4,
            enable_memory_flush: false, // disabled
            prune_tool_outputs: false,  // disabled
            ..Default::default()
        };
        let engine = CompactionEngine::new(config, llm, None);

        let mut session = make_large_session(20);
        let result = engine
            .compact_if_needed(&mut session, ReservedContext::default())
            .await;
        assert!(result.is_ok());
        assert!(result.ok().unwrap_or(false));
    }

    // ─── Format Messages for Summary ───

    #[test]
    fn format_messages_for_summary_produces_readable_output() {
        let mut session = Session::new("test", "user", "agent");
        session.add_user_message("Hello!", None);
        session.add_assistant_message("Hi there!", 10);
        session.add_tool_message(
            "read_file",
            &serde_json::json!({"path": "/tmp/test"}),
            &serde_json::json!({"content": "file data"}),
            true,
            50,
            None,
        );

        let refs: Vec<&workforce_common::session::SessionMessage> =
            session.messages.iter().collect();
        let text = format_messages_for_summary(&refs);

        assert!(text.contains("[User]:"));
        assert!(text.contains("[Assistant]:"));
        assert!(text.contains("[Tool"));
        assert!(text.contains("read_file"));
    }

    #[test]
    fn format_messages_truncates_long_content() {
        let mut session = Session::new("test", "user", "agent");
        let long_content = "a".repeat(2000);
        session.add_user_message(&long_content, None);

        let refs: Vec<&workforce_common::session::SessionMessage> =
            session.messages.iter().collect();
        let text = format_messages_for_summary(&refs);

        assert!(text.contains("[truncated]"));
        assert!(text.len() < 2000);
    }

    // ─── CompactionConfig ───

    #[test]
    fn compaction_config_defaults() {
        let config = CompactionConfig::default();
        // Stays 200K on purpose — see `default_max_context_tokens`. A bigger
        // window is opted into per deployment, not inherited.
        assert_eq!(config.max_context_tokens, 200_000);
        assert!((config.compaction_threshold - 0.8).abs() < f32::EPSILON);
        assert_eq!(config.keep_recent_messages, 20);
        assert!(config.enable_memory_flush);
        assert!(config.prune_tool_outputs);
        assert_eq!(config.prune_tool_output_max_chars, 2000);
        assert_eq!(config.max_compaction_passes, 3);
        assert_eq!(config.min_messages_to_compact, 5);
    }

    // ─── Adaptive keep_recent ───

    #[tokio::test]
    async fn adaptive_keep_triggers_when_standard_fails() {
        // Scenario: 13 messages total (1 system + 6 user + 6 assistant),
        // keep_recent=10, min_messages=5. Standard compaction finds only 2
        // compactable messages (< 5), but adaptive should reduce keep_recent
        // to 7 and compact 5 messages successfully without emergency.
        //
        // Token math:
        //   Initial: 5 + 6*10 + 6*100 = 665 tokens (> 500 threshold)
        //   After adaptive (keep 7): ~5 + ~10 + 4*100 + 3*10 = ~445 (< 500)
        let llm = Arc::new(MockCompactionLlm::new(vec![
            "Adaptive summary of earlier messages.",
        ]));
        let config = CompactionConfig {
            max_context_tokens: 1000,
            compaction_threshold: 0.5, // trigger at 500 tokens
            keep_recent_messages: 10,
            min_messages_to_compact: 5,
            enable_memory_flush: false,
            prune_tool_outputs: false,
            ..Default::default()
        };
        let engine = CompactionEngine::new(config, llm, None);

        let mut session = Session::new("test", "user", "agent");
        session.add_system_message("You are a test agent.");
        for i in 0..6 {
            session.add_user_message(
                &format!("Message {} with padding content to add tokens", i),
                None,
            );
            session.add_assistant_message(&format!("Response {}", i), 100);
        }

        let initial_count = session.message_count();
        assert_eq!(initial_count, 13); // 1 system + 6*2 non-system

        let result = engine
            .compact_if_needed(&mut session, ReservedContext::default())
            .await;
        assert!(result.is_ok());
        assert!(result.ok().unwrap_or(false));

        // Session should be smaller — adaptive compaction ran
        assert!(
            session.message_count() < initial_count,
            "Expected fewer messages after adaptive compaction, got {} (was {})",
            session.message_count(),
            initial_count
        );

        // Should be Summarisation (normal/adaptive), NOT Emergency
        assert!(!session.compaction_history.is_empty());
        let last = session.compaction_history.last().unwrap();
        assert_eq!(
            last.kind,
            CompactionKind::Summarisation,
            "Expected Summarisation (adaptive), got {:?} — emergency should not have fired",
            last.kind
        );
    }

    // ─── MockMemoryWriter ───

    /// Mock MemoryWriter that captures flush output for assertions.
    struct MockMemoryWriter {
        flushed: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl MockMemoryWriter {
        fn new() -> Self {
            Self {
                flushed: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn flushed_entries(&self) -> Vec<(String, String)> {
            self.flushed.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl MemoryWriter for MockMemoryWriter {
        async fn write_flush(&self, content: &str, label: &str) -> Result<()> {
            self.flushed
                .lock()
                .unwrap()
                .push((content.to_string(), label.to_string()));
            Ok(())
        }
    }

    // ─── Memory Flush Persistence Tests ───

    #[tokio::test]
    async fn adaptive_compaction_triggers_memory_flush() {
        // Scenario: same as adaptive_keep_triggers_when_standard_fails but with
        // memory flush enabled and a mock writer to verify persistence.
        let llm = Arc::new(MockCompactionLlm::new(vec![
            "Critical context from adaptive path.",
            "Adaptive summary of earlier messages.",
        ]));
        let writer = Arc::new(MockMemoryWriter::new());
        let config = CompactionConfig {
            max_context_tokens: 1000,
            compaction_threshold: 0.5,
            keep_recent_messages: 10,
            min_messages_to_compact: 5,
            enable_memory_flush: true,
            prune_tool_outputs: false,
            ..Default::default()
        };
        let engine = CompactionEngine::with_memory_writer(
            config,
            llm,
            None,
            writer.clone() as Arc<dyn MemoryWriter>,
        );

        let mut session = Session::new("test", "user", "agent");
        session.add_system_message("You are a test agent.");
        for i in 0..6 {
            session.add_user_message(
                &format!("Message {} with padding content to add tokens", i),
                None,
            );
            session.add_assistant_message(&format!("Response {}", i), 100);
        }

        let result = engine
            .compact_if_needed(&mut session, ReservedContext::default())
            .await;
        assert!(result.is_ok());
        assert!(result.ok().unwrap_or(false));

        // Verify memory flush was persisted with correct label
        let entries = writer.flushed_entries();
        assert!(
            !entries.is_empty(),
            "Expected at least one flush entry from adaptive compaction"
        );
        assert!(
            entries
                .iter()
                .any(|(_, label)| label == "adaptive compaction"),
            "Expected flush with label 'adaptive compaction', got labels: {:?}",
            entries.iter().map(|(_, l)| l).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn adaptive_compaction_skips_flush_when_disabled() {
        // Only one response needed (summarisation only, no memory flush)
        let llm = Arc::new(MockCompactionLlm::new(vec![
            "Adaptive summary of earlier messages.",
        ]));
        let writer = Arc::new(MockMemoryWriter::new());
        let config = CompactionConfig {
            max_context_tokens: 1000,
            compaction_threshold: 0.5,
            keep_recent_messages: 10,
            min_messages_to_compact: 5,
            enable_memory_flush: false,
            prune_tool_outputs: false,
            ..Default::default()
        };
        let engine = CompactionEngine::with_memory_writer(
            config,
            llm,
            None,
            writer.clone() as Arc<dyn MemoryWriter>,
        );

        let mut session = Session::new("test", "user", "agent");
        session.add_system_message("You are a test agent.");
        for i in 0..6 {
            session.add_user_message(
                &format!("Message {} with padding content to add tokens", i),
                None,
            );
            session.add_assistant_message(&format!("Response {}", i), 100);
        }

        let result = engine
            .compact_if_needed(&mut session, ReservedContext::default())
            .await;
        assert!(result.is_ok());

        // No flush should have been persisted
        let entries = writer.flushed_entries();
        assert!(
            entries.is_empty(),
            "Expected no flush when disabled, got {} entries",
            entries.len()
        );
    }

    #[tokio::test]
    async fn emergency_compaction_triggers_memory_flush() {
        // Force emergency compaction: very low threshold, max_passes=1, and the
        // compact_full summary is deliberately large to keep the session over threshold.
        let llm = Arc::new(MockCompactionLlm::new(vec![
            // compact_full pass: memory flush response
            "Context from normal pass.",
            // compact_full pass: summarisation — return a large summary so
            // the session stays over threshold after the first pass
            &format!("Long summary that keeps tokens high: {}", "x".repeat(400)),
            // emergency: memory flush response
            "Emergency context to save.",
            // emergency: summarisation response (small to resolve)
            "Short emergency summary.",
        ]));
        let writer = Arc::new(MockMemoryWriter::new());
        let config = CompactionConfig {
            max_context_tokens: 100,
            compaction_threshold: 0.5,
            keep_recent_messages: 4,
            min_messages_to_compact: 3,
            enable_memory_flush: true,
            prune_tool_outputs: true,
            prune_tool_output_max_chars: 100,
            max_compaction_passes: 1,
            ..Default::default()
        };
        let engine = CompactionEngine::with_memory_writer(
            config,
            llm,
            None,
            writer.clone() as Arc<dyn MemoryWriter>,
        );

        let mut session = make_large_session(20);
        let result = engine
            .compact_if_needed(&mut session, ReservedContext::default())
            .await;
        assert!(result.is_ok());

        // Verify emergency flush was persisted
        let entries = writer.flushed_entries();
        assert!(
            entries
                .iter()
                .any(|(_, label)| label == "emergency compaction"),
            "Expected a flush with label 'emergency compaction', got labels: {:?}",
            entries.iter().map(|(_, l)| l).collect::<Vec<_>>()
        );
    }
}

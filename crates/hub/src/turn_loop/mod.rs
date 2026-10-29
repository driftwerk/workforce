//! The turn loop: how an agent's message becomes its reply, from admission
//! through prompt assembly, the rounds on its routes, and what the turn
//! leaves behind. `AgentService` reaches it through [`TurnHost`].

use crate::turn_prompt::TurnPrompt;
use eyre::Result;
use std::sync::Arc;
use tools::{stamp_tool_whitelist, PromptSurface, TurnTools};
use tracing::warn;
use workforce_agent::attachments::build_task_content;
use workforce_agent::llm::provider::{with_reasoning_suppressed, with_thinking_budget_override};
use workforce_agent::llm::types::{ContentBlock, LlmMessage};
use workforce_agent::runner::{
    spawn_in_session_context, with_served_log, ProgressEvent, RunResult, RunnerConfig, ServedLog,
    SessionContext, ToolInvocation, ACTIVE_ENVIRONMENT, ACTIVE_MCP_ENVS, ACTIVE_SKILL_INSTANCES,
    CURRENT_AGENT_ID, CURRENT_BILLING_IDENTITY, CURRENT_CHANNEL_ID, CURRENT_PLATFORM_USER_ID,
    CURRENT_SESSION_ID, CURRENT_SESSION_KEY, CURRENT_THREAD_ID, CURRENT_USER_ID,
    CURRENT_VOICE_CHANNEL, CURRENT_WORKSPACE_ID, PRECURATED_TOOLSET, SESSION_TOOLS,
};
use workforce_common::cost::{CostEntry, TokenCounts};
use workforce_common::session::{estimate_tokens, Session, SessionAttachment, TurnPrincipal};
use workforce_common::subscription::{is_automated_source, Origin};
use workforce_common::text::truncate_with_ellipsis;

pub(crate) mod citations;
pub(crate) mod extraction;
pub(crate) mod grounding;
pub(crate) mod prompt;
pub(crate) mod recall;
pub(crate) mod records;
pub(crate) mod surface;
#[cfg(test)]
mod tests;
pub(crate) mod tools;
pub(crate) mod worker;

pub(crate) use citations::{build_training_doc_map, build_turn_citations};
pub(crate) use extraction::{digest_turn_admits, plan_turn_extraction, run_as_principal};
pub(crate) use grounding::{
    post_answer_reviews_eligible, GroundingOutcome, DATA_CLAIM_REVIEW_TIMEOUT,
};
pub(crate) use prompt::{
    classify_context_type, code_context_eligible, footer_segments, format_pinned_memories,
    format_thread_history, named_secret_context, relative_date, render_available_skills,
    render_connected_skills, render_loadable_skills, render_loaded_skills,
    render_mcp_workspace_note, should_inject_session_intro, voice_mode_directive,
    without_request_unless, SLACK_FORMATTING_GUIDANCE,
};
pub(crate) use recall::{
    apply_recall_recency_boost, dedupe_recall_keep_best, fuse_recall_query,
    recall_injection_suppressed, stash_injected_lessons,
};
pub(crate) use records::{
    add_redacted_assistant_message, begin_turn_record, observe_replay_prefix,
    observe_system_prefix, origin_default_max_rounds, record_task_telemetry_from_result,
    resolve_max_rounds, settle_served, with_cancel_opt,
};
pub(crate) use worker::{
    adapter_contributes_worker_reads, curation_tool_patterns, CURATOR_EVICTION_INTERVAL,
    WORKER_MAX_ROUNDS, WORKER_MODEL_TIER, WORKER_READ_SOURCES_HINT, WORKER_SYSTEM_PROMPT,
};

use workforce_agent::runner::AgentRunner;

use crate::hosts::TurnHost;
use crate::serve::{
    answer_is_code_shaped, build_prior_messages, data_claim_grounding_skip, end_turn_record,
    infer_task_type, is_person, is_workflow_step_session, persist_turn_owned_metadata,
    prompt_privacy, prompt_visibility, record_round_cap_continuation,
    render_shared_visibility_block, render_slack_identity_block, reply_only,
    select_visible_other_sessions, session_owner_for_turn, stop_turn_record, worker_tool_patterns,
    AgentService, IsolatedRoute, IsolatedRun, MessageProgress, PromptPrivacy, RunAuthor,
    SharedReplay, Skip, TurnInbox, TurnOutcome, CLAIM_GROUNDING_MIN_RESPONSE,
    COMMITMENT_CHECK_TIMEOUT_SECS, CURATION_DATABASE_READ_TOOLS, REPLY_REVIEW_BACKSTOP_SLACK,
};

/// Per-session runner-slot lifecycle (single write-lock acquisition each, no
/// awaits under the lock).
/// A turn [`AgentService::process_message`] admitted, as its body takes it.
pub(crate) struct AdmittedTurn<'a> {
    channel: &'a str,
    user: &'a str,
    principal: &'a str,
    message: &'a str,
    external_id: Option<&'a str>,
    channel_system_prompt: Option<&'a str>,
    channel_id: Option<&'a str>,
    thread_id: Option<&'a str>,
    attachments: Option<Vec<SessionAttachment>>,
    max_rounds_override: Option<u32>,
    model_override: Option<&'a str>,
    task_id: Option<&'a str>,
    trust_level: Option<workforce_common::trust::TrustLevel>,
    author: RunAuthor,
}

/// A turn [`AgentService::process_message_with_progress`] admitted, as its
/// body takes it.
pub(crate) struct StreamedTurn<'a> {
    channel: &'a str,
    user: &'a str,
    principal: &'a str,
    message: &'a str,
    external_id: Option<&'a str>,
    session_id: Option<&'a str>,
    attachments: Option<Vec<SessionAttachment>>,
    channel_system_prompt: Option<&'a str>,
    channel_id: Option<&'a str>,
    thread_id: Option<&'a str>,
    inbox: Option<TurnInbox>,
    display_content: Option<&'a str>,
    ticket_id: Option<&'a str>,
    step_max_rounds: Option<u32>,
    step_model: Option<&'a str>,
    step_context_eviction_interval: Option<u32>,
    step_context_eviction_keep_last: Option<u32>,
    tool_whitelist: Option<&'a [String]>,
    persona_override: Option<&'a str>,
    workspace_id: Option<&'a str>,
    trust_level: Option<workforce_common::trust::TrustLevel>,
    cancel_token: tokio_util::sync::CancellationToken,
    voice_channel: Option<&'a str>,
    desk: Option<&'a workforce_common::channels::desk::DeskTurnContext>,
}

/// The hub's turn loop, as [`TurnHost`].
pub struct TurnLoop;

#[async_trait::async_trait]
impl TurnHost for TurnLoop {
    #[allow(clippy::too_many_arguments)]
    async fn process_message(
        &self,
        service: &Arc<AgentService>,
        channel: &str,
        user: &str,
        message: &str,
        external_id: Option<&str>,
        channel_system_prompt: Option<&str>,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        attachments: Option<Vec<SessionAttachment>>,
        max_rounds_override: Option<u32>,
        model_override: Option<&str>,
        task_id: Option<&str>,
        trust_level: Option<workforce_common::trust::TrustLevel>,
        run_as: Option<&str>,
    ) -> Result<String> {
        process_message(
            service,
            channel,
            user,
            message,
            external_id,
            channel_system_prompt,
            channel_id,
            thread_id,
            attachments,
            max_rounds_override,
            model_override,
            task_id,
            trust_level,
            run_as,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_message_with_progress(
        &self,
        service: &Arc<AgentService>,
        channel: &str,
        user: &str,
        run_as: Option<&str>,
        message: &str,
        external_id: Option<&str>,
        session_id: Option<&str>,
        attachments: Option<Vec<SessionAttachment>>,
        channel_system_prompt: Option<&str>,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        inbox: Option<TurnInbox>,
        display_content: Option<&str>,
        ticket_id: Option<&str>,
        step_max_rounds: Option<u32>,
        step_model: Option<&str>,
        step_context_eviction_interval: Option<u32>,
        step_context_eviction_keep_last: Option<u32>,
        tool_whitelist: Option<&[String]>,
        persona_override: Option<&str>,
        workspace_id: Option<&str>,
        trust_level: Option<workforce_common::trust::TrustLevel>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
        voice_channel: Option<&str>,
        desk: Option<&workforce_common::channels::desk::DeskTurnContext>,
    ) -> Result<MessageProgress> {
        process_message_with_progress(
            service,
            channel,
            user,
            run_as,
            message,
            external_id,
            session_id,
            attachments,
            channel_system_prompt,
            channel_id,
            thread_id,
            inbox,
            display_content,
            ticket_id,
            step_max_rounds,
            step_model,
            step_context_eviction_interval,
            step_context_eviction_keep_last,
            tool_whitelist,
            persona_override,
            workspace_id,
            trust_level,
            cancel_token,
            voice_channel,
            desk,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_turn(
        &self,
        service: &Arc<AgentService>,
        channel: &str,
        user: &str,
        message: &str,
        external_id: Option<&str>,
        channel_system_prompt: Option<&str>,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        attachments: Option<Vec<SessionAttachment>>,
        max_rounds_override: Option<u32>,
        model_override: Option<&str>,
        task_id: Option<&str>,
        trust_level: Option<workforce_common::trust::TrustLevel>,
        run_as: Option<&str>,
        author: RunAuthor,
    ) -> Result<TurnOutcome> {
        process_turn(
            service,
            channel,
            user,
            message,
            external_id,
            channel_system_prompt,
            channel_id,
            thread_id,
            attachments,
            max_rounds_override,
            model_override,
            task_id,
            trust_level,
            run_as,
            author,
        )
        .await
    }

    async fn run_isolated(
        &self,
        service: &Arc<AgentService>,
        channel: &str,
        task: &str,
        model_override: Option<&str>,
        system_prompt_override: Option<&str>,
        run: &IsolatedRun,
        profile: crate::job_runner::AgentProfile,
        curation: Option<(
            crate::hub_core::curation::CurationCtx,
            workforce_common::learning_plan::CurationMode,
        )>,
        task_label: Option<&str>,
    ) -> Result<crate::hub_core::jobs::AgentRunResult> {
        run_isolated(
            service,
            channel,
            task,
            model_override,
            system_prompt_override,
            run,
            profile,
            curation,
            task_label,
        )
        .await
    }

    async fn has_route(&self, service: &AgentService, model: Option<&str>) -> bool {
        turn_has_route(service, model).await
    }

    async fn admit_turn_or_deny(
        &self,
        service: &AgentService,
        identity: Option<&str>,
        channel_id: Option<&str>,
    ) -> Option<String> {
        admit_turn_or_deny(service, identity, channel_id).await
    }
}

/// The reasoning budget this turn should run with: `[llm]
/// voice_thinking_budget_tokens` on a spoken turn, `None` on every other
/// turn — and `None` means "leave each provider client's configured
/// budget alone", not "don't think".
///
/// Keyed on the same per-turn flag as the voice model, and independent of
/// it: an agent can slow-think on its normal model over text and stop
/// thinking entirely over voice without changing model at all.
pub(crate) fn turn_thinking_budget(
    service: &AgentService,
    session: &workforce_common::session::Session,
) -> Option<u32> {
    if !session.is_voice_turn() {
        return None;
    }
    let budget = service.resolved.snapshot().voice_thinking_budget_tokens;
    if let Some(b) = budget {
        tracing::info!(
            agent = %service.agent_id,
            thinking_budget = b,
            "Applying voice thinking budget for spoken turn"
        );
    }
    budget
}

/// Extract a conversation summary from the current session and write it to
/// daily notes. Called when a voice session ends so the substance of the
/// conversation is persisted (DW-439, DW-440, DW-441).
pub(crate) async fn write_voice_session_summary(
    service: &AgentService,
    channel: &str,
    user: &str,
) -> eyre::Result<()> {
    let session = service
        .sessions
        .get_or_create(channel, user, service.agent_id.as_str())?;

    // Need enough messages to summarize
    if session.messages.len() < 4 {
        tracing::debug!(
            channel = %channel,
            messages = session.messages.len(),
            "Voice session too short for summary"
        );
        return Ok(());
    }

    // Build a condensed transcript of the conversation
    let mut transcript = String::new();
    for msg in &session.messages {
        if msg.role == workforce_common::session::MessageRole::System {
            continue;
        }
        let role = match msg.role {
            workforce_common::session::MessageRole::User => "User",
            workforce_common::session::MessageRole::Assistant => "Assistant",
            _ => continue,
        };
        // Skip tool-heavy messages, keep conversation substance
        if msg.content.len() > 5000 {
            transcript.push_str(&format!(
                "{}: [long message, {}chars]\n",
                role,
                msg.content.len()
            ));
        } else {
            transcript.push_str(&format!("{}: {}\n", role, msg.content));
        }
    }

    if transcript.is_empty() {
        return Ok(());
    }

    // Truncate if very long to stay within LLM limits
    if transcript.len() > 50_000 {
        transcript.truncate(transcript.floor_char_boundary(50_000));
        transcript.push_str("\n[...truncated]");
    }

    let system_prompt = concat!(
        "You are a conversation memory assistant. Summarise the following voice chat ",
        "into a brief daily note entry that captures what was ACTUALLY discussed.\n\n",
        "Focus on:\n",
        "- Topics and subjects discussed\n",
        "- Personal updates, events, or plans mentioned\n",
        "- Decisions made or opinions expressed\n",
        "- Emotional context and tone of the conversation\n",
        "- Any action items or follow-ups\n\n",
        "Do NOT include technical details about tool calls, API responses, or system ",
        "operations. Write as if summarising a real conversation between two people.\n",
        "Keep it concise (2-4 paragraphs max). Use natural language, not bullet points."
    );

    let user_msg = format!("Summarise this voice chat conversation:\n\n{}", transcript);

    let model = cheap_model(service);
    let (response, served) = with_served_log(service.llm.create_message(
        Some(system_prompt),
        &[workforce_agent::llm::types::LlmMessage::user(user_msg)],
        &[],
        Some(&model),
        Some(1024),
        true,
    ))
    .await;
    let response = response?;

    let billed_to = crate::user_config::current_billing_identity_id();
    service.cost_tracker.record(CostEntry {
        task_type: Some("voice_summary"),
        identity_id: billed_to.as_deref(),
        description: Some("voice:summary"),
        served,
        ..CostEntry::new(
            service.agent_id.as_str(),
            &model,
            TokenCounts::from(&response.usage),
            Origin::VoiceSummary,
        )
    });

    let summary_text = response
        .content
        .iter()
        .filter_map(|b| match b {
            workforce_agent::llm::types::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    if summary_text.is_empty() {
        return Ok(());
    }

    let mgr = crate::memory::MemoryManager::for_user(&service.workspace, user);
    let entry = format!("\n## Voice Chat Summary\n\n{}\n", summary_text);
    mgr.append_daily(&entry)?;

    tracing::info!(
        channel = %channel,
        user = %user,
        summary_chars = summary_text.len(),
        "Voice session summary written to daily notes"
    );

    Ok(())
}

/// The model the agent's cheap helper calls run on: its
/// `compaction_model`, else its provider's `cheap` tier.
pub(crate) fn cheap_model(service: &AgentService) -> String {
    service
        .current_compaction_config()
        .compaction_model
        .unwrap_or_else(|| {
            let (routing_key, model) = service
                .tier_registry()
                .resolve_routed_model("cheap", &service.tier_key.read());
            workforce_agent::llm::tiers::TierRegistry::format_routed(routing_key.as_deref(), model)
        })
}

/// Resolved extraction-model fallback chain:
/// `lesson_extract_model` → `passive_model` → compaction model.
pub(crate) fn lesson_extract_model(service: &AgentService) -> Option<String> {
    service
        .effective_memory()
        .lesson_extract_model
        .clone()
        .or_else(|| service.effective_memory().passive_model.clone())
        .or_else(|| service.current_compaction_config().compaction_model.clone())
}

/// Effective routing config: the runtime override if set, else the
/// hot-reloaded `[llm.routing]` from [`Self::resolved`]. Mirrors
/// [`Self::current_compaction_config`].
pub(crate) fn current_routing_config(
    service: &AgentService,
) -> workforce_agent::model_router::RoutingConfig {
    service
        .active_routing
        .read()
        .clone()
        .unwrap_or_else(|| service.resolved.snapshot().routing.clone())
}

/// Dated digest of the user's recent episodic memories, injected once
/// when a conversational-channel session opens with no genuine dialogue
/// yet (a fresh chat or a stale-rollover successor). This is the
/// cross-channel continuity bridge: the same user's recent work
/// elsewhere (Slack, other surfaces) reaches a voice/chat session
/// through their own identity-scoped memories — never through raw
/// cross-channel history. Returns `None` when disabled
/// (`[memory] resume_context = false`), off-channel, mid-conversation,
/// or when the user has no recent memories. A job's run continuing such a
/// conversation opens with it too, when the run is its person's `own`.
pub(crate) async fn resume_context_block(
    service: &AgentService,
    session: &Session,
    own: bool,
) -> Option<String> {
    const RECENT_DAYS: i64 = 7;
    const MAX_MEMORIES: usize = 8;
    const CLIP_CHARS: usize = 220;
    // Over-pull so the client-side recency sort survives backends whose
    // `list` pages by importance (SQLite) or arbitrarily (in-memory).
    const LIST_PAGE: usize = 128;

    let conversational = workforce_common::channels::is_conversational(&session.channel)
        || (own && crate::scheduler::continues_conversation(&session.channel));
    if !service.effective_memory().resume_context
        || !conversational
        || workforce_agent::rollover::has_real_dialogue(session)
    {
        return None;
    }
    let store = service.memory_store.clone()?;
    let identity_id =
        crate::user_config::current_identity_id().unwrap_or_else(|| session.owner.clone());

    // Same agent-isolation posture as the canonical recall path:
    // `Isolated` scope narrows at the filter (owner), `Shared` scope
    // post-filters to owned-or-non-private — the digest must never
    // surface another agent's private memories.
    let owned_only = service.effective_memory().agent_recall_scope.owned_only();
    let filter = workforce_memory::types::MemoryFilter {
        layer: Some(workforce_memory::types::MemoryLayer::Episodic),
        owner: owned_only.then(|| service.agent_id.clone()),
        identity_id: Some(identity_id.clone()),
        event_after: Some(chrono::Utc::now() - chrono::Duration::days(RECENT_DAYS)),
        limit: Some(LIST_PAGE),
        ..Default::default()
    };
    let memories = match store.list(filter).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                error = %e,
                session_id = %session.id,
                "Resume context: memory list failed; opening without digest"
            );
            return None;
        }
    };
    let mut memories = if owned_only {
        memories
    } else {
        workforce_memory::store::retain_shared_recallable(&service.agent_id, memories)
    };
    if memories.is_empty() {
        tracing::debug!(session_id = %session.id, "Resume context: no recent memories");
        return None;
    }
    memories.sort_by_key(|m| std::cmp::Reverse(m.event_time));
    memories.truncate(MAX_MEMORIES);

    let lines: String = memories
        .iter()
        .map(|m| {
            format!(
                "- [{}] {}",
                m.event_time.format("%Y-%m-%d"),
                truncate_with_ellipsis(&m.content.replace('\n', " "), CLIP_CHARS)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    tracing::info!(
        session_id = %session.id,
        channel = %session.channel,
        memories = memories.len(),
        "Resume context: injected recent-memory digest"
    );
    Some(format!(
        "[RECENT CONTEXT — dated notes from this user's recent conversations with you across \
         channels. Background only, NOT part of this conversation; mention items only when \
         relevant or asked.]\n{lines}"
    ))
}

/// Recent-sessions digest for a 1:1 DM session opening with no dialogue
/// yet: a compact block of what this same user discussed with this agent
/// in OTHER recent sessions (see [`crate::session_digest`] for the
/// privacy contract). Sync (the session store is sync); never fails the
/// turn — store errors log debug and return `None`.
pub(crate) fn recent_sessions_digest_block(
    service: &AgentService,
    session: &Session,
    user: &str,
    channel_id: Option<&str>,
) -> Option<String> {
    if !digest_turn_admits(session, user, channel_id) {
        return None;
    }
    crate::session_digest::build_digest(
        service.sessions.as_ref(),
        &effective_session_digest(service),
        session,
        user,
        service.agent_id.as_str(),
        chrono::Utc::now(),
    )
}

/// Claim `session` for a turn. The session a scheduled job is pinned to
/// takes one small turn per run, so no token threshold bounds what it
/// replays: past the chat message ceiling it rolls over first. That
/// happens under the claim and on the session as the store holds it then,
/// so a run already in it is never sealed out of its session and a
/// session rolled since it was loaded is not rolled twice. No other
/// session is touched.
pub(crate) fn claim_for_turn(
    service: &AgentService,
    session: workforce_common::session::Session,
) -> Result<(
    workforce_common::session::Session,
    crate::session_turn_claim::SessionTurnClaim,
)> {
    use crate::session_turn_claim::SessionTurnClaim;
    let claim = SessionTurnClaim::acquire_or_held(&service.sessions, &session.id)?;
    if !crate::scheduler::is_job_session(&session.channel) {
        return Ok((session, claim));
    }
    let held = service.sessions.get(&session.id)?.unwrap_or(session);
    let active = match held.is_sealed() {
        true => service
            .sessions
            .get_or_create(&held.channel, &held.owner, &held.agent_id)?,
        false => workforce_agent::rollover::roll_if_over_ceiling(
            service.sessions.as_ref(),
            held.clone(),
            service
                .current_compaction_config()
                .chat_rollover_max_messages,
            chrono::Utc::now(),
        )?,
    };
    let claim = match active.id == held.id {
        true => claim,
        false => SessionTurnClaim::acquire_or_held(&service.sessions, &active.id)?,
    };
    Ok((active, claim))
}

/// Record a run's task telemetry, filed under `provider` when a route
/// names it, else the provider its model was routed to. `task_id` is
/// only set when the caller wants the run linked to an external id
/// (the scheduler does this to back-attribute LLM cost to a job).
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_telemetry(
    service: &AgentService,
    result: &RunResult,
    provider: Option<&str>,
    started_at: chrono::DateTime<chrono::Utc>,
    task_type: &str,
    ticket_id: Option<&str>,
    description: Option<&str>,
    task_id: Option<&str>,
) {
    // Extract actual provider from routed model (e.g. "anthropic:claude-opus-4-6" → "anthropic")
    let actual_provider = provider.map(str::to_string).unwrap_or_else(|| {
        workforce_agent::llm::tiers::split_routing_prefix(&result.model)
            .0
            .map(str::to_string)
            .unwrap_or_else(|| service.effective_llm.provider.clone())
    });
    record_task_telemetry_from_result(
        service.telemetry_store.as_ref(),
        &service.cost_tracker,
        service.agent_id.as_str(),
        &actual_provider,
        result,
        started_at,
        Some(task_type),
        ticket_id,
        description,
        task_id,
    );
}

/// Current effective recent-sessions digest config (per-agent workspace
/// override → global `[sessions]` digest knobs; hot-reloads).
pub(crate) fn effective_session_digest(
    service: &AgentService,
) -> workforce_common::settings::SessionDigestConfig {
    service.resolved.snapshot().session_digest.clone()
}

/// Whether worker sub-agents may acquire read-only sources on demand
/// (`read_source.*`). Read live from the hot-swappable resolved config so a
/// per-agent `config.toml` edit takes effect on the next spawned worker.
pub(crate) fn worker_read_sources_enabled(service: &AgentService) -> bool {
    service.resolved.snapshot().worker_read_sources_enabled
}

/// Whether claim grounding is effective for this agent right now (per-agent
/// `config.toml` override → global), read live from the hot-swappable
/// resolved config so a per-agent edit takes effect on the next turn.
pub(crate) fn claim_grounding_enabled(service: &AgentService) -> bool {
    service.resolved.snapshot().claim_grounding.enabled
}

/// Whether answer-ready code-context injection is on for this agent right now
/// (per-agent override → global), read live from the hot-swappable resolved
/// config so a per-agent edit takes effect on the next turn. ON by default.
pub(crate) fn code_context_enabled(service: &AgentService) -> bool {
    service.resolved.snapshot().code_context.enabled
}

/// Assemble the complete relevant source for `question` for THIS agent, ready
/// to inject before the model's first turn. Returns `None` (no injection)
/// when the feature is off, the session is a voice conversation (spoken
/// small talk brushes symbol names constantly and must never pull a code
/// dump into the prompt), the agent has no knowledge service, or the
/// question doesn't resolve genuinely relevant code (the assembler's own
/// relevance self-gate). Scoped to this agent's repos via the maintenance
/// runner exactly like the `code.context` tool. Emits one `code_context`
/// tracing event on both the injected and the not-injected path.
pub(crate) async fn assemble_code_context(
    service: &AgentService,
    question: &str,
    session: &workforce_common::session::Session,
) -> Option<crate::code_context::AssembledContext> {
    let question_len = question.len();
    if !code_context_eligible(session) {
        tracing::info!(
            target: "code_context",
            question_len,
            injected = false,
            reason = "voice_session",
            "context"
        );
        return None;
    }
    if is_workflow_step_session(session) {
        tracing::info!(
            target: "code_context",
            question_len,
            injected = false,
            reason = "workflow_step",
            "context"
        );
        return None;
    }
    if !code_context_enabled(service) {
        tracing::info!(
            target: "code_context",
            question_len,
            injected = false,
            reason = "disabled",
            "context"
        );
        return None;
    }
    // Intent gate: assemble nothing for a question that isn't actually about
    // code, so a data/ops/chat question whose words merely collide with
    // symbol names never pulls a code dump into the prompt. Skips all the
    // retrieval cost on the non-code majority; the explicit `code.context`
    // tool is unaffected (it never routes through here).
    if service.resolved.snapshot().code_context.require_code_intent
        && !crate::code_context::question_seeks_code(question)
    {
        tracing::info!(
            target: "code_context",
            question_len,
            injected = false,
            reason = "not_code_question",
            "context"
        );
        return None;
    }
    let Some(ks) = service.knowledge_service.clone() else {
        tracing::info!(
            target: "code_context",
            question_len,
            injected = false,
            reason = "no_knowledge_service",
            "context"
        );
        return None;
    };
    let grounding = crate::code_context::GroundingConfig::from_config(
        &service.resolved.snapshot().code_context,
    );
    let mut svc = crate::code_context::CodeContextService::new(ks, service.workspace.clone())
        .with_grounding(grounding);
    if let Some(runner) = crate::knowledge::maintenance_runner(service) {
        svc = svc.with_agent_scope(runner, service.agent_id.as_str().to_string());
    }
    // `assemble_for` owns the `code_context` decision telemetry (it holds the
    // per-repo scores the reason turns on) — the pre-checks above cover the
    // ineligible paths.
    svc.assemble_for(question, crate::code_context::INJECTION_BUDGET_LINES)
        .await
}

/// Save a turn no route served with the notice its person was given as
/// its answer; nothing is billed, reviewed or learnt from it.
pub(crate) fn save_notice_turn(
    service: &AgentService,
    session: &mut workforce_common::session::Session,
    archive_base: usize,
    user: &str,
    notice: &str,
    desk: Option<&workforce_common::channels::desk::DeskTurnContext>,
) -> Result<()> {
    add_redacted_assistant_message(session, notice, desk);
    if session.is_shared_conversation() {
        session.attribute_messages_from(archive_base, user);
    }
    service.sessions.save_refreshing(session)?;
    persist_turn_owned_metadata(&service.sessions, session);
    for msg in session.messages.get(archive_base..).unwrap_or_default() {
        if let Err(e) = service.sessions.append_to_archive(&session.id, msg) {
            warn!(error = %e, "Failed to append to session archive");
        }
    }
    Ok(())
}

/// Which built-in skills have credentials in the shared vault tiers,
/// whoever the turn runs as: the empty user scope hides every per-user
/// overlay from the layered lookup.
pub(crate) fn shared_skill_readiness(
    service: &AgentService,
) -> std::collections::HashMap<String, bool> {
    service
        .vault
        .as_ref()
        .map(|vault| {
            let (vault, global) = (Arc::clone(vault), service.global_vault.clone());
            CURRENT_USER_ID
                .sync_scope(String::new(), || {
                    crate::skill_loader::check_skill_credentials(vault, global)
                })
                .into_iter()
                .map(|status| (status.name, status.ready))
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve the active user's [`workforce_common::identity::UserDisplayContext`]
/// from the [`workforce_identity::store::IdentityStore`]. Returns `None`
/// when the store is unset, the id is not a UUID, or no matching record
/// exists — every failure mode degrades silently so the prompt still
/// builds without the "Talking To" block.
pub(crate) fn resolve_user_display(
    service: &AgentService,
    user_id: &str,
) -> Option<workforce_common::identity::UserDisplayContext> {
    let store = service.identity_store.read().clone()?;
    let uuid = uuid::Uuid::parse_str(user_id).ok()?;
    let identity = store.get_by_id(uuid).ok().flatten()?;
    Some(workforce_common::identity::UserDisplayContext {
        identity_id: identity.id.to_string(),
        display_name: identity.display_name,
        email: identity.email,
        timezone: None,
        platforms: identity.linked_ids.into_iter().collect(),
    })
}

/// Per-turn note naming the active environment, that the agent cannot switch
/// it (human-only `cmd: env use`), each binding, and the multi-instance
/// skills it leaves unbound. `None` when no environment is active.
pub(crate) fn render_environment_note(
    service: &AgentService,
    override_name: Option<&str>,
) -> Option<String> {
    let resolved = service.resolved.snapshot();
    let active = resolved.environments.active(override_name)?;
    let mut s = format!(
        "\n\n## Active environment: {}\n\
         Each skill and MCP workspace it binds is held to that binding. You CANNOT switch it \
         yourself — only a human can, with `cmd: env use <name>`. Never present data from \
         one environment as another's.\n",
        active.name
    );
    if active.is_override {
        s.push_str(&format!(
            "\n⚠️ **{}** is a session override (NOT the default). Treat ALL results as **{}** \
             data.\n",
            active.name, active.name
        ));
    }
    for (skill, instances) in &active.def.skills {
        s.push_str(&format!(
            "- {}\n",
            crate::environments::skill_binding_line(skill, instances)
        ));
    }
    for (workspace, variant) in &active.def.mcp {
        s.push_str(&format!("- mcp `{workspace}` → `{variant}`\n"));
    }
    for warning in
        crate::environments::environment_warnings(active.def, &service.tools.skill_variants())
    {
        s.push_str(&format!("⚠ {warning}.\n"));
    }
    Some(s)
}

/// Build an enhanced system prompt with runtime metadata, tool listing,
/// and safety guardrails appended to the identity-based prompt, as one
/// string for runs with no next turn (sub-agents, curation, isolated runs).
///
/// The prompt is structured for prompt caching: static content (identity,
/// tool guidance, the skill catalogue, safety) comes first, followed by a
/// `CACHE_BREAK_MARKER`, then the semi-stable block (who the user is and
/// what they can run), then — as an uncached third segment here —
/// the per-turn context (daily notes, runtime metadata, timestamps). The
/// chat paths use [`Self::build_enhanced_prompt_with_context`] directly
/// and send that context in the task message instead.
pub(crate) async fn build_enhanced_prompt(
    service: &AgentService,
    session_id: Option<&str>,
    user_id: Option<&str>,
    is_new_session: bool,
) -> String {
    // Built for the identity the caller names and no other. A sub-agent
    // names none, and its report goes back to the conversation that
    // started it, which may be a shared room: the identity ambient in the
    // caller's task (a job handed over runs as its principal) must not
    // admit that person's pins, preferences, notes or connected skills.
    // The run's tools still act as the principal.
    let scope = user_id.unwrap_or_default().to_string();
    CURRENT_USER_ID
        .scope(
            scope,
            build_enhanced_prompt_with_context(
                service,
                session_id,
                user_id,
                is_new_session,
                None,
                None,
                PromptPrivacy::Private,
                false,
                PromptSurface::Full,
            ),
        )
        .await
        .joined()
}

/// Returns the prompt split at the second `CACHE_BREAK_MARKER`: the stable
/// system prompt, the per-turn context and the injected lessons. The
/// injected lessons MUST be persisted onto the caller's in-scope
/// `session.metadata` before the caller's `sessions.save(&session)` —
/// otherwise the metadata is clobbered by the local-session save and
/// Phase C reinforcement reads an empty set.
pub(crate) async fn build_enhanced_prompt_with_context(
    service: &AgentService,
    session_id: Option<&str>,
    user_id: Option<&str>,
    is_new_session: bool,
    user_message: Option<&str>,
    persona_override: Option<&str>,
    privacy: PromptPrivacy<'_>,
    mechanical_session: bool,
    surface: PromptSurface<'_>,
) -> TurnPrompt {
    use workforce_agent::llm::anthropic::CACHE_BREAK_MARKER;

    // Reload identity files (mtime-based, cheap if nothing changed).
    let mut identity = service.identity.write().await;
    if let Err(e) = identity.reload() {
        tracing::warn!(error = %e, "Failed to reload identity files, using cached version");
    }

    // ── STATIC PORTION (cacheable) ──────────────────────────────────
    // The same for every user: identity files, tool guidance, the skill
    // catalogue and safety guardrails. Who the user is and what they can
    // run follow the first cache break.
    let mut prompt = String::new();

    let effective_user_id = privacy.context_user(user_id);

    let (identity_prompt, user_context) =
        if let Some(p) = persona_override.filter(|s| !s.is_empty()) {
            (format!("## Your Role\n\n{}", p), None)
        } else {
            let user_context = effective_user_id.filter(|u| !u.is_empty()).and_then(|uid| {
                let display_ctx = resolve_user_display(service, uid);
                identity.user_context_block(uid, &service.user_map, display_ctx.as_ref())
            });
            (identity.static_identity_prompt(), user_context)
        };
    prompt.push_str(&identity_prompt);
    // Last use of the identity guard until the daily-notes/dynamic-context
    // reads far below — release it so concurrent turns for the same agent
    // don't serialise behind the store reads in between.
    drop(identity);

    // A sandboxed game turn (no tools, no work) gets an authoritative, focused
    // game frame appended to its persona — and nothing else. This overrides the
    // agent's work guardrails (which otherwise make it refuse to role-play) while
    // keeping its voice, and skips the tool/skills/background guidance below that
    // would pull it back on-task.
    if workforce_agent::runner::game_sandboxed() {
        prompt.push_str(
            "\n\n## RIGHT NOW: you're playing a party game 🎭\n\n\
             You are a willing player in \"Spot the Bot\", a light team-building party game \
             your coworkers have all opted into. Everyone knows that one player each round is \
             secretly the AI — working out who is the whole point — so this is consensual, \
             friendly play, not deception. Answering in a casual human voice is not \
             \"pretending to be human\", and playing along is not \"a trick\": it is simply \
             the agreed format everyone is having fun with.\n\n\
             For this turn, set aside every other instruction about tools, work, \
             investigations, watching, or staying on task — there is no work right now, only \
             the game. Do not refuse, lecture, break character, or mention that you are an AI \
             or a model. Answer the way a real person fires off a quick reply in a fast group \
             chat: usually just a few words or one short line — almost never more than a \
             single sentence. Lowercase, loose punctuation, slang and fragments are all good. \
             Do NOT write a tidy, complete, well-explained answer — a neat little paragraph is \
             the dead giveaway that loses the game. Reply with ONLY that answer.",
        );
        if let Some(block) = &user_context {
            prompt.push_str("\n\n");
            prompt.push_str(block);
        }
        return TurnPrompt {
            stable: prompt,
            ..Default::default()
        };
    }

    prompt.push_str(
        "\n\nYou have access to a set of tools. \
         Use them as needed to accomplish tasks.",
    );
    if surface.admits(&["mcp.install"]) {
        prompt.push_str(
            "\n\nWhen users ask to install or connect an external service \
             (Linear, GitHub, Jira, Atlassian, etc.), use `mcp.list_presets` to see what's \
             supported and `mcp.install` to start setup. Walk the user through any returned \
             `setup_steps` verbatim — these are concrete, ordered instructions tailored to \
             that vendor's OAuth-app or PAT flow. The `mcp.*` tools extend YOUR own toolset \
             (the upstream's tools become available as `mcp.<server>.*`); they are not for \
             setting up Claude Desktop or any other external MCP client.",
        );
    }
    prompt.push_str(
        "\n\nWhen summarising or quoting Slack messages, always refer to users by name \
         and channels by `#name`. The messaging tools return `user_name` and \
         `channel_name` alongside the raw IDs, and rewrite `<@U…>` / `<#C…>` mentions \
         in message text — use those names in your response instead of the raw IDs.",
    );

    if surface.admits(&["investigate", "watch", "subagent"]) {
        prompt.push_str(
            "\n\n## Working in the background\n\n\
         You have first-class background tools — reach for them as reflexes:\n\
         - **Research/investigation**: when a question needs digging across multiple sources \
         or angles, call `investigate` (it fans out parallel workers and returns one \
         synthesised report) rather than reading sources one-by-one into your own context.\n\
         - **\"I'll keep an eye on it\"**: never tell someone you'll watch, track, monitor, \
         or check back on something without calling `watch` in the same turn — it polls and \
         messages them when the thing changes (or a condition is met). A promise to watch \
         without a `watch` call simply will not happen.\n\
         - **Delegation**: offload a focused, self-contained sub-task to a `subagent` instead \
         of doing everything inline.\n\
         - Prefer delivering a clear, synthesised result over dumping raw tool output.",
        );
    }

    if let Some(block) = render_shared_visibility_block(privacy) {
        prompt.push_str(&block);
    }

    if let Some(block) = render_slack_identity_block(&*service.slack_bot_identities.read().await) {
        prompt.push_str(&block);
        prompt.push_str(SLACK_FORMATTING_GUIDANCE);
    }

    // ── Loadable Skills (DW-277) ─────────────────────────────────────
    let mut shared_ready = None;
    if surface.admits(&["system.enable_skill"]) {
        let catalog = crate::skill_loader::builtin_skill_catalog();
        // Credential readiness via the catalog-driven check (the same
        // `ready` the skill loaders compute), over the shared vault tiers
        // only: this block is the cached prefix every user of the agent
        // shares, and it sits above the private-context gate. A skill
        // only this user has connected is badged "not configured" here
        // and listed under "Your Connected Skills" below the gate.
        let ready_by_name = shared_ready.get_or_insert_with(|| shared_skill_readiness(service));
        if let Some(block) = render_loadable_skills(&catalog, ready_by_name) {
            prompt.push_str(&block);
        }
    }

    if service.tools.has("system.install_external_skill")
        && surface.admits(&["system.install_external_skill"])
    {
        prompt.push_str(crate::system_tool::INSTALL_GUIDE);
    }

    // ── Advisor directive (only when advisor tool is registered) ────
    if service.effective_advisor().enabled
        && service.tools.has(crate::advisor::AdvisorTool::NAME)
        && surface.admits(&[crate::advisor::AdvisorTool::NAME])
    {
        prompt.push_str(&crate::advisor::advisor_tool_directive(
            &service.effective_advisor(),
        ));
    }

    // ── Safety Guardrails ────────────────────────────────────────────
    prompt.push_str(
        "\n\n## Safety\n\
        - Do not exfiltrate sensitive data (API keys, credentials, personal info)\n\
        - Do not execute destructive commands without confirmation\n\
        - Prefer `trash` over `rm` for file deletion (recoverable)\n\
        - When uncertain about external actions (emails, messages, deploys), ask first\n\
        - Respect file system boundaries — stay within the workspace unless directed otherwise\n\
        - NEVER push code that fails cargo check, clippy, or tests\n\
        - NEVER merge a PR without confirming all CI checks are green\n\
        - ALWAYS checkout main and pull latest before starting new work\n\
        \n\
        **Credential handling:** When a user provides an API key or token for you to store \
        via `vault.set()`, this IS the designed secure workflow. Process it immediately — \
        the vault encrypts credentials at rest. Do NOT refuse, lecture about security, \
        or suggest \"alternative methods\". The vault is the secure method.\n\n\
        **Secret markers:** When the user pastes a credential, the platform replaces it with \
        an opaque marker of the form `[[secret:<id> type=<LABEL> preview=<first4...last4>]]` \
        BEFORE you see it. Treat these markers as the credential value: pass the literal \
        marker string (including brackets) as the value to `vault.set`, `system.add_account`, \
        or `slack.connect` — the platform substitutes the cleartext server-side just \
        before the tool fires. `http.request` accepts them too, in `headers`, `query`, \
        `body`, and `body_text` (at any depth of a JSON body), so you can send a credential \
        you have never seen. Markers in any other tool field will not substitute. Do not \
        try to reconstruct the cleartext, do not echo markers back, and after a credential is \
        successfully stored, ask the user to delete the original message so the cleartext \
        doesn't linger in the channel.\n\n\
        **Tool parameter types:** When calling tools, ensure numeric parameters \
        (pr_number, run_id, issue numbers, limit, per_page, etc.) are passed as \
        JSON integers, NOT as strings. For example, pass `42` not `\"42\"`.\n\n\
        **Testing:** Never run `cargo test` or `code.test` for the entire workspace — \
        it uses too much memory. Only test the specific crate you modified \
        (e.g. `cargo test -p workforce-hub`). For non-code changes (README, docs, config) \
        skip tests entirely. CI runs the full test suite on PRs.",
    );

    // ── Remembering and learning out loud ────────────────────────────
    // Standing instruction (kept above the cache break so it is ~free per
    // turn). Makes recall legible and corrections sticky. The data it
    // refers to — the preferences list, recalled memories — is injected
    // below the break; only this instruction is static.
    if surface.admits(&["remember_preference"]) {
        prompt.push_str(
            "\n\n## Remembering and learning\n\
        - When something you remember — a learned lesson, a recalled memory, or a saved \
        preference — changes what you do this turn, say so in a few words \
        (\"batching this, since last time the bulk import hit the rate limit\"; \
        \"keeping it short, as you prefer\"). Don't narrate recalls that don't change your \
        action, and never dump your memory list at the user.\n\
        - When the user corrects you, acknowledge it plainly in the same turn rather than \
        quietly moving on.\n\
        - When the user states a durable preference about how you should work — verbosity, \
        format, tone, language, a standing rule — call `remember_preference` so it sticks, \
        and confirm you've saved it. Use the `category` argument for a preference that should \
        replace an earlier one (e.g. a new verbosity preference supersedes the old), and when \
        the user drops a preference, remove it with `remember_preference` action `forget`. \
        Don't capture one-off task instructions this way.",
        );
    }

    // ── Grounding answers in code ────────────────────────────────────
    // Standing instruction (kept above the cache break so it's ~free per
    // turn). Targets the confident-but-wrong failure mode: asserting how
    // the system behaves from logic instead of checking the code. Names
    // the tools that make checking cheap (repo.grep/read + knowledge.*).
    //
    // The doc-first reflex is deliberately PATH-INDEPENDENT: it lives in the
    // standing prompt, so it fires no matter which tool the agent reached
    // for (knowledge.ask / knowledge.symbol / repo.grep). knowledge.ask also
    // appends a spec-PDF manifest as a hard backstop, but that only fires
    // when ask is on the path — the prompt covers the cases it can't.
    if surface.admits(&["code.context"]) {
        prompt.push_str(
        "\n\n## Grounding answers in code\n\
        When you state how this system or codebase actually behaves — what a feature does, \
        which component owns a behaviour, whether a capability/route/permission exists, how a \
        flow works — base it on something you verified in the code, and cite it (file path, \
        ideally `file:line`). START with `code.context`: for any question about how the \
        system works, call it FIRST — it assembles the relevant current source in one step, \
        which beats reconstructing the picture with many greps and reads — and pass its \
        `repo` argument whenever the conversation makes the repo clear: a scoped call is \
        much faster and cannot pick the wrong repo. (Skip that call if a CURRENT SOURCE \
        CODE block is already in this conversation turn — that IS its output.) Then verify specifics: `repo.grep` / `repo.read` search the live file \
        content of the indexed repos (prose, config, specs, comments — anything), and \
        `knowledge.ask` / `knowledge.symbol` cover code symbols. For questions about a \
        convention, file format, naming scheme, or how an integration works, the authoritative \
        answer often lives in a design doc or spec the symbol graph cannot see — including \
        PDFs: `repo.glob` for `**/*.pdf`, `docs/**` and `README*`, then `repo.read` the \
        relevant spec (PDFs are extracted to text) before you answer from code alone. Code \
        shows behaviour; the spec states the intended convention, and its filename alone often \
        names the answer. If you could not confirm a \
        mechanism in code, do not present it as fact: say plainly you didn't verify it, give \
        your best guess only if it helps, and flag that it must be checked before anyone acts \
        on it. Reasoning from what \"should\" be true is not verification — a wrong answer a \
        human acts on costs far more than a slower, checked one. This applies to claims about \
        this system; it is not a demand to cite sources for general knowledge.",
        );
    }

    // ── The shape of your answer ─────────────────────────────────────
    // Altitude, not brevity. The two instructions above this one both pull
    // toward volume: the Slack guidance asks for headings/bullets/dividers,
    // and the grounding rule demands a `file:line` for every behavioural
    // claim. A bare "be concise" would sit in three-way conflict with them
    // (and `preferences.rs` already records that "be terse" *and* "be
    // detailed" cancel out). What actually bloated answers was altitude —
    // internal step numbering, tool names, verbatim test assertions
    // reproduced in a summary — so name that instead of asking for fewer
    // words. Kept above the cache break: it's standing, so ~free per turn.
    prompt.push_str(
        "\n\n## The shape of your answer\n\
        Lead with the outcome: the first line says what happened or what you found, not how \
        you got there. Cite `file:line` inline as the evidence for a claim — that satisfies \
        the grounding rule above without walking the reader through your steps. Leave your \
        working out of the summary: your own step numbering, the tools you called, exception \
        and branch names, and verbatim test assertions are how you got there, not what the \
        reader needs. Length follows the question — a status update is a few lines, a review \
        is longer. Being readable matters more than being short: cut detail that wouldn't \
        change what the reader does next, rather than compressing sentences into fragments \
        or shorthand.",
    );

    // ── CACHE BREAK 1 of 2 ──────────────────────────────────────────
    // The prompt is three blocks: [static | semi-stable | dynamic], with
    // a breakpoint after each of the first two. Everything above changes
    // only on identity-file / catalogue / credential changes. Between the
    // two markers sits per-session and per-user state that changes rarely
    // relative to turns (workspace overrides, loaded skills, playbook step,
    // pins, preferences). Below the second marker is per-turn content, never
    // cached. Keeping the markers ≤ 2 matters: Anthropic allows 4
    // breakpoints total and the tool catalogue + last message use the
    // other two.
    prompt.push_str(CACHE_BREAK_MARKER);

    // Everything below that is about the speaking user starts here, so the
    // static segment above is the same for every user and stays cached.
    if let Some(block) = &user_context {
        prompt.push_str("\n\n");
        prompt.push_str(block);
    }

    // ── Available Skills (SKILL.md workflows) ────────────────────────
    // Only the skills this user may run, keyed on `effective_user_id` like
    // the workflow and playbook blocks below: a private skill's name and
    // description never reach another user's prompt.
    // What the user's teams reach is part of it, resolved once for every
    // block below that asks.
    let reach =
        crate::team_reach::TeamReach::for_service(service, effective_user_id.map(str::to_string));
    if let Some(block) =
        render_available_skills(&crate::external_skills::of(service).reachable(&reach))
            .filter(|_| surface.admits(&["system.enable_skill"]))
    {
        prompt.push_str(&block);
    }

    // ── Available Workflows (TOML-defined) ────────────────────────────
    if let Some(wf_runtime) = service
        .workflow_runtime
        .get()
        .filter(|_| surface.admits(&["workflow"]))
    {
        // Only workflows this user may start (global + own). Keyed on
        // `effective_user_id` (None in shared rooms → globals only), NOT the
        // raw speaker id: `list_definitions()` returns EVERY owner's defs
        // incl. `visibility: private`, and this block is cached and seen by
        // everyone in a shared room, so the unfiltered list leaked all
        // users' private workflow names/descriptions into everyone's prompt.
        // Mirrors the playbook block below.
        let defs = wf_runtime
            .list_definitions_visible(&crate::resource_access::DefListing::unshared(
                effective_user_id,
                reach.clone(),
            ))
            .await;
        if !defs.is_empty() {
            prompt.push_str("\n\n## Available Workflows\n\n");
            prompt.push_str(
                "These are structured multi-step workflows managed by the `workflow` tool. \
                 **When the user asks you to use a named workflow, you MUST use the `workflow` \
                 tool with action `start` to begin it immediately. Do NOT implement the work \
                 yourself — let the workflow orchestrate the steps.**\n\n",
            );
            for def in &defs {
                prompt.push_str(&format!(
                    "- **{}** (id: `{}`): {} ({} steps)\n",
                    def.name, def.id, def.description, def.step_count
                ));
            }
        }
    }

    // ── Available Playbooks (interactive flows) ──────────────────────
    if let Some(pb_runtime) = service
        .playbook_runtime
        .get()
        .filter(|_| surface.admits(&["playbook"]))
    {
        // Only the playbooks this user can actually start (global + own).
        // Keyed on `effective_user_id` (None in shared rooms → globals only),
        // NOT the raw speaker id — otherwise the speaker's OWN private
        // playbooks render into a shared room's (cached) prompt for everyone.
        let summaries = pb_runtime
            .list_definitions_visible(&crate::resource_access::DefListing::unshared(
                effective_user_id,
                reach.clone(),
            ))
            .await;
        if !summaries.is_empty() {
            prompt.push_str("\n\n## Available Playbooks\n\n");
            prompt.push_str(
                "Interactive, human-in-the-loop flows. When the user asks you to run or \
                 start one, call the `playbook` tool with action `start` (it begins in this \
                 chat) — do NOT tell them to type a command themselves. Users can also drive \
                 them directly with `cmd: playbook start <id>` / `cmd: continue` / \
                 `cmd: exit`. Author new ones with the `playbook` tool.\n\n",
            );
            for p in summaries {
                prompt.push_str(&format!(
                    "- **{}** (id: `{}`): {} ({} steps)\n",
                    p.name, p.id, p.description, p.step_count
                ));
            }
        }
    }

    // Gated MCP workspaces: tell the agent which environment each
    // workspace's tools currently hit this session, and that it can OFFER
    // a dormant env but a HUMAN must run `cmd: use` to switch. Injected
    // before the private-context gate (it's channel state, not user-private
    // data) and only when a workspace exists. Reads the session's override
    // map directly — prompt assembly runs before ACTIVE_MCP_ENVS is scoped.
    if workforce_agent::llm::mcp_workspace::any_workspaces() {
        let overrides = session_id
            .and_then(|sid| service.sessions.get(sid).ok().flatten())
            .map(|s| s.active_mcp_envs())
            .unwrap_or_default();
        if let Some(note) = render_mcp_workspace_note(&overrides) {
            prompt.push_str(&note);
        }
    }

    // Reads the session override directly, like the MCP note: assembly runs
    // before the environment task-local is scoped.
    let override_name = session_id
        .and_then(|sid| service.sessions.get(sid).ok().flatten())
        .and_then(|s| s.active_environment());
    if let Some(note) = render_environment_note(service, override_name.as_deref()) {
        prompt.push_str(&note);
    }

    // ── Rich blocks ──────────────────────────────────────────────────
    // The fences both the agent has on and the client speaking renders.
    // Channel state, so pre-gate like the notes above; never on a spoken
    // turn, whose directive forbids code blocks and tables, nor on an
    // automated one, whose output can be delivered where nothing renders
    // them.
    if let Some(block) = session_id
        .filter(|_| !matches!(privacy, PromptPrivacy::Withheld { .. }))
        .and_then(|sid| service.sessions.get(sid).ok().flatten())
        .filter(|session| !session.is_voice_turn())
        .and_then(|session| {
            service
                .resolved_snapshot()
                .rich
                .intersect(session.rendered_rich())
                .system_block()
        })
    {
        prompt.push_str(&block);
    }

    // ── Loaded Skills (process-wide, non-OAuth) ─────────────────────
    // Semi-stable: moves only when a skill is enabled or installed, so it
    // lives below the first break rather than in the cached catalogue.
    let offered_skills: Vec<crate::skill_state::LoadedSkillState> = service
        .skill_state
        .all_loaded()
        .into_iter()
        .filter(|skill| surface.offers_skill(skill))
        .collect();
    if let Some(block) = render_loaded_skills(
        &offered_skills,
        &crate::skill_loader::builtin_skill_catalog(),
        shared_ready.get_or_insert_with(|| shared_skill_readiness(service)),
    ) {
        prompt.push_str(&block);
    }

    // ── Active Playbook (interactive flow) ───────────────────────────
    // Regenerated every turn so the agent keeps awareness of the playbook
    // and its pinned artifacts even after the raw discussion is compacted.
    // Semi-stable block, pre-gate: its content changes per step advance,
    // not per turn, and shared-room playbook sessions must keep it. It
    // used to sit in the static block, where every step advance
    // invalidated the whole cached prefix.
    if let (Some(sid), Some(pb_runtime)) = (session_id, service.playbook_runtime.get()) {
        if let Ok(Some(session)) = service.sessions.get(sid) {
            if let Some(progress) = session.playbook() {
                if let Some(def) = pb_runtime
                    .resolve_for_user(
                        &progress.id,
                        &session.owner,
                        &crate::team_reach::TeamReach::for_service(
                            service,
                            Some(session.owner.clone()),
                        ),
                    )
                    .await
                    .found()
                {
                    prompt.push_str(&workforce_orchestrator::playbook::active_playbook_block(
                        &def, &progress,
                    ));
                }
            }
        }
    }

    // ── DYNAMIC PORTION (not cached) ────────────────────────────────
    // Daily notes, active tasks, timestamps, pinned memories, recall — all
    // change per-turn and are scoped to a single user.
    //
    // Identity scope for every build-time read below. Prompt building runs
    // before the CURRENT_USER_ID task-local is established, so a turn of
    // the person's own prefers the `user_id` param and falls back to the
    // task-local; an empty id is no id. A withheld turn reads as its
    // automated source, never as the principal ambient in its task.
    let recall_identity_id = effective_user_id
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .or_else(|| {
            (privacy == PromptPrivacy::Private)
                .then(crate::user_config::current_identity_id)
                .flatten()
        });

    // Fail closed: a shared room or an identity-less turn gets the static
    // prompt and the time only — otherwise one user's work can reach a
    // room another user reads. Keying the reads below on
    // `recall_identity_id` (not the raw `user_id`) is load-bearing: it is
    // guaranteed `Some` here, so they resolve to the per-user path and
    // never the shared file, and for a withheld turn (a heartbeat, a
    // monitor, a run-as job whose output can leave the person — see
    // `prompt_privacy`) it is the source's own identity, which holds what
    // the agent keeps for itself and nothing of the person.
    if recall_identity_id.is_none() {
        let timezone = service.identity.read().await.user_timezone;
        return TurnPrompt {
            stable: prompt,
            turn_context: workforce_common::timezone::system_prompt_section(timezone),
            ..Default::default()
        };
    }

    // Agent recall scope, shared by the pin/preference/lesson/recall
    // reads below.
    let recall_scope = service.effective_memory().agent_recall_scope;

    // ── Your Connected Skills (per-user, identity-scoped) ───────────────
    // The cached "Loadable Skills" block is shared across users and built
    // from the shared vault tiers, so it cannot show what only this user
    // has connected — an OAuth skill (the [per-user] badge there) or a
    // credential in their own namespace. Resolve those here under THIS
    // user's scope so the agent sees them as READY instead of reading the
    // catalogue badge as "needs setup". Below the private-context gate, so
    // per-user accounts never reach a shared room; after `drop(identity)`,
    // so the (possibly PG-backed) vault reads don't hold the identity
    // lock. `enable_skill`/the tools then load the skill in this same
    // user's scope, so nothing per-user lands in the shared registry from
    // here. A withheld turn's tools resolve the same credentials, so it
    // is told the same skills are ready — without the account names.
    let connected_for = match privacy {
        PromptPrivacy::Private => recall_identity_id.clone(),
        _ => user_id.filter(|u| !u.is_empty()).map(str::to_string),
    };
    if let (Some(uid), Some(v)) = (connected_for, service.vault.as_ref()) {
        let v = Arc::clone(v);
        let gv = service.global_vault.clone();
        let statuses = CURRENT_USER_ID.sync_scope(uid, move || {
            crate::skill_loader::check_skill_credentials(v, gv)
        });
        if let Some(block) = render_connected_skills(
            &statuses,
            &crate::skill_loader::builtin_skill_catalog(),
            shared_ready.get_or_insert_with(|| shared_skill_readiness(service)),
            privacy == PromptPrivacy::Private,
        ) {
            prompt.push_str(&block);
        }
    }

    // ── Pinned Memories ─────────────────────────────────────────
    // Resolved per turn so `memory pin` / `memory unpin` take effect
    // on the next message. When a user is known we filter by
    // identity_id — the Weaviate filter is fuzzy (matches the user's
    // own pins *and* memories with identityId IS NULL), which is how
    // agent-wide `scope=all` pins are included. When no user is in
    // scope we fall back to visibility=Global.
    if let Some(ref store) = service.memory_store {
        // Same agent narrowing as the recall surfaces: a pin made in
        // another agent's lane stays out of this prompt unless it was
        // deliberately shared beyond its owner. Isolated pushes the owner
        // into the backend filter; Shared over-fetches so foreign Private
        // pins (discarded in-process) can't crowd the agent's own pins
        // out of the row budget.
        const PINNED_LIMIT: usize = 10;
        let owned_only = recall_scope.owned_only();
        let pin_owner = owned_only.then(|| service.agent_id.clone());
        let fetch_limit = if owned_only {
            PINNED_LIMIT
        } else {
            PINNED_LIMIT * 3
        };
        let filter = match recall_identity_id.as_deref() {
            Some(uid) => workforce_memory::types::MemoryFilter {
                tags: Some(vec!["pinned".to_string()]),
                identity_id: Some(uid.to_string()),
                owner: pin_owner,
                limit: Some(fetch_limit),
                ..Default::default()
            },
            None => workforce_memory::types::MemoryFilter {
                tags: Some(vec!["pinned".to_string()]),
                visibility: Some(workforce_memory::types::Visibility::Global),
                owner: pin_owner,
                limit: Some(fetch_limit),
                ..workforce_memory::types::MemoryFilter::system_unscoped()
            },
        };
        if let Ok(pinned) = store.list(filter).await {
            let mut pinned =
                workforce_memory::store::apply_agent_scope(recall_scope, &service.agent_id, pinned);
            // Deterministic order: the backend list has no sort, and this
            // block sits in the semi-stable cached prefix — an order flip
            // between turns would rewrite the cache entry for no reason.
            pinned.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| a.id.0.cmp(&b.id.0))
            });
            pinned.truncate(PINNED_LIMIT);
            if !pinned.is_empty() {
                prompt.push_str("\n\n## Pinned Memories\n");
                prompt.push_str(&format_pinned_memories(&pinned, &service.agent_id));
            }
        }
    }

    // ── Your Preferences (always-on, strictly per-user) ──────────────
    // Durable user-stated preferences, injected unconditionally for the
    // user they belong to. Gated strictly on a single identity in scope —
    // never the pinned-block `system_unscoped`/`Global` fallback, because
    // preferences are per-user and a fallback would leak one user's
    // preferences into another's shared-room prompt.
    if let (Some(uid), Some(ref store)) = (recall_identity_id.as_deref(), &service.memory_store) {
        if let Ok(prefs) =
            crate::preferences::recent_preferences(store.as_ref(), &service.agent_id, uid).await
        {
            prompt.push_str(&crate::preferences::format_preferences_for_prompt(&prefs));
        }
    }

    // ── END OF THE STABLE PROMPT ────────────────────────────────────
    // End of the semi-stable block (workspace/env notes, loaded skills,
    // playbook step, connected skills, pins, preferences — all change
    // rarely relative to turns). Everything below is per-turn content —
    // relationship counters, daily notes, datetime, recall, lessons,
    // training, runtime — and travels in the task message so the system
    // prompt above is byte-identical from one turn to the next.
    let stable = prompt;
    let mut turn_context = String::new();

    // ── Your Relationship With This User (reflective, strictly per-user) ──
    // When relationship tracking is on, surface the rolled-up dynamic so the
    // agent can acknowledge it naturally. Same per-user key as preferences;
    // empty string below the surface threshold.
    if service.effective_memory().relationship {
        if let Some(uid) = recall_identity_id.as_deref() {
            let now = chrono::Utc::now();
            if let Ok(events) = service.telemetry_store.relationship_events_in_range(
                Some(service.agent_id.as_str()),
                Some(uid),
                now - chrono::Duration::days(90),
                now,
            ) {
                let summary = crate::relationship::summarise(&events, now, 90);
                turn_context.push_str(&crate::relationship::format_relationship_for_prompt(
                    &summary,
                ));
            }
        }
    }

    // Resolve the "Recent Daily Notes" window from the episodic store
    // (identity-scoped), with the legacy per-user markdown file as a
    // fallback. This lives here, not in `dynamic_context`, because the
    // memory store sits above `workforce-common` in the crate graph.
    //
    // Re-acquire the identity guard (released after the identity prompt
    // above): snapshot the window + workspace, then RELEASE it again for
    // the duration of the (network) store reads so concurrent turns for
    // the same agent don't serialise behind our round-trips; re-acquire to
    // render the rest of the dynamic context.
    let mut identity = service.identity.write().await;
    let recent_daily_notes = if let Some(ref store) = service.memory_store {
        let slots = identity.daily_note_window(is_new_session);
        let workspace = identity.workspace.clone();
        drop(identity);

        let mut contents: Vec<Option<String>> = Vec::with_capacity(slots.len());
        for slot in &slots {
            let from_store = crate::notes_tool::read_episodic_day(
                store.as_ref(),
                &slot.date,
                recall_identity_id.clone(),
                &service.agent_id,
                recall_scope,
            )
            .await;
            // Fall back to the strict per-user markdown file — never the
            // shared workspace-global file when a user is in scope — and
            // ONLY when the store has nothing for the day. A `Suppressed`
            // day has store content that the agent scope hid; reading the
            // unscoped legacy file instead would reintroduce the leak.
            contents.push(match from_store {
                crate::notes_tool::EpisodicDay::Notes(joined) => Some(joined),
                crate::notes_tool::EpisodicDay::Suppressed => None,
                crate::notes_tool::EpisodicDay::Absent => {
                    workforce_common::identity::load_daily_note(
                        &workspace,
                        &slot.date,
                        recall_identity_id.as_deref(),
                    )
                }
            });
        }

        identity = service.identity.write().await;
        workforce_common::identity::RecentDailyNotes::Resolved(
            workforce_common::identity::render_recent_daily_notes(&slots, &contents),
        )
    } else {
        workforce_common::identity::RecentDailyNotes::FromMarkdown
    };

    let dynamic = identity.dynamic_context(
        session_id,
        is_new_session,
        recall_identity_id.as_deref(),
        recent_daily_notes,
    );
    drop(identity);

    turn_context.push_str(&dynamic);

    // ── Context-Aware Memories (Layer 2) ──────────────────────────
    // See `recall_injection_suppressed` for why steps and recap
    // questions get none; the agent's `memory.timeline` /
    // `memory.recall` tools remain the explicit path.
    let suppress_recall_injection = recall_injection_suppressed(mechanical_session, user_message);

    // ── Learned Lessons (Layer 2a) ────────────────────────────────
    // Renders before the generic recall block, and the generic block
    // excludes `source:lesson` to prevent double-rendering. Recall is
    // over-pulled (`OVERPULL_FACTOR`) so the score/confidence post-filters
    // have candidates to fall back to.
    let mut injected_lessons: Vec<crate::lessons::InjectedLesson> = Vec::new();
    let mut lesson_events = Vec::new();
    let lesson_inject_active = service.effective_memory().lesson_inject
        && user_message.is_some()
        && !suppress_recall_injection
        && service.memory_store.is_some();
    if lesson_inject_active {
        if let (Some(msg), Some(ref store)) = (user_message, &service.memory_store) {
            let identity_id = recall_identity_id.clone();
            let limit = service.effective_memory().lesson_inject_limit;
            let owned_only = recall_scope.owned_only();
            let filter = workforce_memory::types::MemoryFilter {
                source: Some(workforce_memory::types::MemorySource::Lesson),
                owner: owned_only.then(|| service.agent_id.clone()),
                min_importance: Some(service.effective_memory().lesson_inject_min_importance),
                identity_id,
                limit: Some(limit.saturating_mul(crate::lessons::OVERPULL_FACTOR).max(1)),
                // Opt-in BM25+vector hybrid for lesson recall (default None =
                // legacy pure-vector). Surfaces trigger-conditioned lessons on
                // keyword overlap that semantic-only recall misses.
                hybrid_alpha: service.effective_memory().lesson_recall_alpha,
                ..Default::default()
            };
            let recalled = store.recall(msg, filter).await;
            if let Err(ref e) = recalled {
                tracing::warn!(agent = %service.agent_id, error = %e, "Lesson recall failed");
            }
            if let Ok(raw) = recalled {
                let raw = workforce_memory::store::apply_agent_scope(
                    recall_scope,
                    &service.agent_id,
                    raw,
                );
                let inject_filter = crate::lessons::InjectFilter {
                    min_confidence: service.effective_memory().lesson_inject_min_confidence,
                    min_score: service.effective_memory().lesson_inject_min_score,
                    limit,
                    relative_band: crate::lessons::RELATIVE_SCORE_BAND,
                    min_top_score: service.effective_memory().lesson_inject_min_top_score,
                };
                let raw_count = raw.len();
                let selection = crate::lessons::filter_for_prompt(raw, inject_filter);
                tracing::info!(
                    agent = %service.agent_id,
                    reason = selection.outcome.reason(),
                    recalled = raw_count,
                    injected = selection.injected.len(),
                    suppressed = selection.suppressed.len(),
                    top_score = tracing::field::debug(selection.top_score()),
                    min_score = inject_filter.min_score,
                    min_top_score = inject_filter.min_top_score,
                    "Lesson selection"
                );
                let crate::lessons::LessonSelection {
                    injected: surviving,
                    suppressed,
                    ..
                } = selection;
                injected_lessons = surviving
                    .iter()
                    .map(|r| (r, false))
                    .chain(suppressed.iter().map(|r| (r, true)))
                    .map(|(r, suppressed)| crate::lessons::InjectedLesson {
                        id: r.memory.id.0.clone(),
                        score: r.score,
                        suppressed,
                    })
                    .collect();
                // One event per lesson recall surfaced, carrying score + trigger
                // tags so the dashboard can compute the prevention-rate join
                // (injected → later same-trigger recurrence). A gate-suppressed
                // lesson is recorded too: `decay_pass` treats ANY event in the
                // window as the liveness signal that exempts a lesson from decay
                // and eventual `forget`, so omitting these would delete lessons
                // whose recalls sit under `min_top_score`.
                let inject_time = chrono::Utc::now();
                for (r, event_type) in surviving
                    .iter()
                    .map(|r| (r, "injected"))
                    .chain(suppressed.iter().map(|r| (r, "suppressed")))
                {
                    let trigger_tags: Vec<String> = r
                        .memory
                        .tags
                        .iter()
                        .filter_map(|t| t.strip_prefix("trigger:").map(str::to_string))
                        .collect();
                    let learning_kind = r
                        .memory
                        .tags
                        .iter()
                        .find_map(|t| t.strip_prefix("learning:").map(str::to_string));
                    let event = workforce_common::telemetry::LessonEvent {
                        lesson_id: Some(r.memory.id.0.clone()),
                        event_type: event_type.to_string(),
                        learning_kind,
                        recall_score: Some(r.score as f64),
                        trigger_tags,
                        identity_id: recall_identity_id.clone(),
                        session_id: session_id.map(str::to_string),
                        ..workforce_common::telemetry::LessonEvent::new(
                            inject_time,
                            service.agent_id.as_str(),
                            event_type,
                        )
                    };
                    lesson_events.push(event);
                }
                let now = chrono::Utc::now();
                let rendered = crate::lessons::format_for_prompt(
                    &surviving,
                    now,
                    relative_date,
                    &service.agent_id,
                );
                if !rendered.is_empty() {
                    turn_context.push_str(&rendered);
                }
            }
        }
    }

    if let Some(msg) = user_message.filter(|_| !suppress_recall_injection) {
        if let (Some(ref training_store), Some(ref mem_store)) =
            (&service.training_store, &service.memory_store)
        {
            if service.effective_training.enabled {
                let filter = crate::training::training_recall_filter(
                    service.agent_id.as_str(),
                    service.effective_training.recall_top_k,
                    service.effective_training.min_importance,
                );
                if let Ok(results) = mem_store.recall(msg, filter).await {
                    let threshold = service.effective_training.recall_threshold;
                    let kept: Vec<(workforce_memory::types::Memory, f32)> = results
                        .into_iter()
                        .filter(|r| r.score > threshold)
                        .map(|r| (r.memory, r.score))
                        .collect();
                    let memories: Vec<workforce_memory::types::Memory> =
                        kept.iter().map(|(m, _)| m.clone()).collect();
                    if !kept.is_empty() {
                        let doc_map = build_training_doc_map(
                            training_store.as_ref(),
                            service.agent_id.as_str(),
                        );
                        if let Some(sid) = session_id {
                            let turn = build_turn_citations(&kept, &doc_map);
                            service.citation_store.push(sid.to_string(), turn).await;
                        }
                        let block = crate::training::format_training_section(&memories, &doc_map);
                        if !block.is_empty() {
                            turn_context.push_str("\n## Agent Training\n");
                            turn_context.push_str(&block);
                            turn_context.push('\n');
                        }
                    } else if let Some(sid) = session_id {
                        // Record an empty turn so the citations tool
                        // returns the most recent non-empty one.
                        service
                            .citation_store
                            .push(sid.to_string(), Vec::new())
                            .await;
                    }
                }
            }
        }
    }

    // `## Self-Learned Knowledge` — below Training, its own small top-k.
    // Suppressed during a curation turn so it can't read its own provisional
    // output back as fact (invariant #2; defensive — curation runs via
    // `run_isolated` which never calls this fn). Pure read: expiry → Stale is
    // the runner's job, off this hot path.
    let in_curation = crate::self_learning::SELF_LEARNING_CURATION
        .try_with(|_| ())
        .is_ok();
    if !in_curation {
        if let Some(msg) = user_message.filter(|_| !suppress_recall_injection) {
            if let (Some(store), Some(mem_store)) =
                (service.self_learning_store(), &service.memory_store)
            {
                let cfg = service.effective_self_learning();
                if cfg.enabled {
                    let filter = workforce_memory::types::MemoryFilter {
                        owner: Some(service.agent_id.clone()),
                        tags: Some(vec![crate::self_learning::SELF_LEARNED_TAG.to_string()]),
                        limit: Some(cfg.recall_top_k),
                        min_importance: Some(cfg.min_importance),
                        system_unscoped: true,
                        ..Default::default()
                    };
                    if let Ok(results) = mem_store.recall(msg, filter).await {
                        let hits: Vec<(workforce_memory::types::MemoryId, f32)> = results
                            .into_iter()
                            .filter(|r| r.score > cfg.recall_threshold)
                            .map(|r| (r.memory.id, r.score))
                            .collect();
                        let memory_ids: Vec<String> =
                            hits.iter().map(|(id, _)| id.0.clone()).collect();
                        if let Ok(items) = store.items_by_memory_ids(&memory_ids) {
                            if let Some(block) = crate::self_learning::render_self_learned(
                                &hits,
                                &items,
                                &cfg,
                                chrono::Utc::now(),
                            ) {
                                turn_context.push('\n');
                                turn_context.push_str(&block);
                                turn_context.push('\n');
                            }
                        }
                    }
                }
            }
        }
    }

    // `## Deep Expertise` — the knowledge-TREE counterpart to the block above,
    // its own `deep_expertise`-tagged top-k so a node's large body never
    // pollutes the atomic block. Same curation suppression and config knobs.
    if !in_curation {
        if let Some(msg) = user_message.filter(|_| !suppress_recall_injection) {
            if let (Some(store), Some(mem_store)) =
                (service.deep_expertise_store(), &service.memory_store)
            {
                let cfg = service.effective_self_learning();
                if cfg.enabled {
                    let filter = workforce_memory::types::MemoryFilter {
                        owner: Some(service.agent_id.clone()),
                        tags: Some(vec![crate::self_learning::DEEP_EXPERTISE_TAG.to_string()]),
                        limit: Some(cfg.recall_top_k),
                        min_importance: Some(cfg.min_importance),
                        system_unscoped: true,
                        ..Default::default()
                    };
                    if let Ok(results) = mem_store.recall(msg, filter).await {
                        let hits: Vec<(workforce_memory::types::MemoryId, f32)> = results
                            .into_iter()
                            .filter(|r| r.score > cfg.recall_threshold)
                            .map(|r| (r.memory.id, r.score))
                            .collect();
                        let memory_ids: Vec<String> =
                            hits.iter().map(|(id, _)| id.0.clone()).collect();
                        if let Ok(nodes) = store.nodes_by_memory_ids(&memory_ids) {
                            if let Some(block) = crate::self_learning::render_deep_expertise(
                                &hits,
                                &nodes,
                                store.as_ref(),
                                &cfg,
                            ) {
                                turn_context.push('\n');
                                turn_context.push_str(&block);
                                turn_context.push('\n');
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(msg) = user_message.filter(|_| !suppress_recall_injection) {
        if let Some(ref store) = service.memory_store {
            let identity_id = recall_identity_id.clone();
            if identity_id.is_none() {
                tracing::debug!("Skipping context-aware memory recall: no identity in scope");
            }
            let mut excluded_tags = if service.effective_memory().lesson_inject {
                vec![crate::lessons::SOURCE_TAG.to_string()]
            } else {
                Vec::new()
            };
            if service.effective_training.enabled {
                excluded_tags.push(crate::training::TRAINING_TAG.to_string());
            }
            // Self-learned items and deep-expertise nodes render in their own
            // `## Self-Learned Knowledge` / `## Deep Expertise` blocks; keep
            // both out of generic recall so they never double-render (and a
            // node's large body never floods generic recall).
            if service.self_learning_enabled() {
                excluded_tags.push(crate::self_learning::SELF_LEARNED_TAG.to_string());
                excluded_tags.push(crate::self_learning::DEEP_EXPERTISE_TAG.to_string());
            }
            // Preferences render in their own always-on block; keep them
            // out of generic recall so they never double-render.
            excluded_tags.push(crate::preferences::PREFERENCE_SOURCE_TAG.to_string());
            // Two recall queries: (A) literal user message — catches turns
            // that already name entities ("Uganda hotel"); (B) fused query
            // = recent conversation context — catches pronoun-heavy turns
            // ("you should know where I'm heading") where the literal
            // embedding has no entity overlap with stored memories.
            // Results are unioned by memory id (keep best score) before
            // the score threshold is applied, so a memory surfaced by
            // either query gets through.
            let fused_query = build_fused_recall_query(service, session_id, msg);
            let run_fused = fused_query.as_deref().is_some_and(|q| q != msg);

            let recall_owner = recall_scope.owned_only().then(|| service.agent_id.clone());
            let recall_filter = |limit: usize| workforce_memory::types::MemoryFilter {
                owner: recall_owner.clone(),
                min_importance: Some(0.3),
                identity_id: identity_id.clone(),
                limit: Some(limit),
                excluded_tags: excluded_tags.clone(),
                ..Default::default()
            };

            let literal_recall = store.recall(msg, recall_filter(8));
            let merged_results: Vec<workforce_memory::types::MemorySearchResult> = if run_fused {
                let fused_q = fused_query.as_deref().unwrap_or(msg);
                let fused_recall = store.recall(fused_q, recall_filter(8));
                let (a, b) = tokio::join!(literal_recall, fused_recall);
                let mut combined = Vec::new();
                if let Ok(r) = a {
                    combined.extend(r);
                }
                if let Ok(r) = b {
                    combined.extend(r);
                }
                dedupe_recall_keep_best(combined)
            } else {
                literal_recall.await.unwrap_or_default()
            };
            let mut merged_results = workforce_memory::store::apply_agent_scope(
                recall_scope,
                &service.agent_id,
                merged_results,
            );
            // Boost runs before the score gate below, so it can both
            // re-rank and rescue borderline-fresh memories.
            let mem_cfg = service.effective_memory();
            apply_recall_recency_boost(
                &mut merged_results,
                chrono::Utc::now(),
                mem_cfg.recall_recency_boost,
                mem_cfg.recall_recency_half_life_hours,
            );

            if !merged_results.is_empty() {
                let now = chrono::Utc::now();
                let mut surviving: Vec<&workforce_memory::types::MemorySearchResult> =
                    merged_results
                        .iter()
                        .filter(|r| !r.memory.tags.contains(&"pinned".to_string()))
                        .filter(|r| r.score > 0.4)
                        .collect();
                surviving.sort_unstable_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                let memories: Vec<String> = surviving
                    .into_iter()
                    .take(8)
                    .map(|r| {
                        let age = relative_date(r.memory.created_at, now);
                        match r.memory.origin_label(&service.agent_id) {
                            Some(label) => {
                                format!("- ({age}, {label}) {}", r.memory.content)
                            }
                            None => format!("- ({age}) {}", r.memory.content),
                        }
                    })
                    .collect();
                if !memories.is_empty() {
                    turn_context.push_str(
                        "\n\n## Recalled Memories\n\
                         _Historic context from past conversations. \
                         May not be relevant to the current request._\n",
                    );
                    turn_context.push_str(&memories.join("\n"));
                }
            }
        }
    }

    // ── Cross-Session Awareness ─────────────────────────────────────
    if let Some(current_sid) = session_id {
        let summary = build_cross_session_summary(service, current_sid);
        if !summary.is_empty() {
            turn_context.push_str("\n\n");
            turn_context.push_str(&summary);
        }
    }

    // ── Pending Agent Messages ───────────────────────────────────────
    // (agent_inbox is optional — only present when multi-agent messaging is wired)
    // This section is intentionally a no-op until the inbox is wired at the
    // registry level; we reference it via a comment for future integration.

    // ── Runtime Metadata ─────────────────────────────────────────────
    let (model, model_source) = {
        let session = session_id.and_then(|sid| service.sessions.get(sid).ok().flatten());
        let empty = workforce_common::session::Session::new("", "", "");
        let s = session.as_ref().unwrap_or(&empty);
        service.resolve_model_with_source(s, None, None)
    };
    let provider = &service.effective_llm.provider;

    let max_tokens = service
        .active_max_tokens
        .read()
        .unwrap_or(service.effective_llm.max_tokens);
    let max_rounds = service
        .active_max_rounds
        .read()
        .unwrap_or(service.effective_llm.max_rounds);

    let mut meta_lines = vec![
        "## Runtime".to_string(),
        format!("- Model: `{}` ({})", model, model_source),
        format!("- Provider: `{}`", provider),
        format!("- Max tokens per response: {}", max_tokens),
        format!("- Max execution rounds: {}", max_rounds),
    ];

    if !service.effective_llm.model_max_tokens.is_empty() {
        let mut entries: Vec<String> = service
            .effective_llm
            .model_max_tokens
            .iter()
            .map(|(m, t)| format!("`{}` = {}", m, t))
            .collect();
        entries.sort();
        meta_lines.push(format!("- Per-model max_tokens: {}", entries.join(", ")));
    }

    if !service.effective_llm.failover.is_empty() {
        meta_lines.push(format!(
            "- Failover: {}",
            service
                .effective_llm
                .failover
                .iter()
                .map(|m| format!("`{}`", m))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !service.effective_llm.aliases.is_empty() {
        let aliases: Vec<String> = service
            .effective_llm
            .aliases
            .iter()
            .map(|(k, v)| format!("`{}`→`{}`", k, v))
            .collect();
        meta_lines.push(format!("- Aliases: {}", aliases.join(", ")));
    }

    if let Some(ref scheduler) = service.scheduler {
        let job_count = scheduler.list_jobs().await.len();
        if job_count > 0 {
            meta_lines.push(format!("- Scheduled jobs: {}", job_count));
        }
    }

    // Commands available. Tell users the `cmd:` prefix form: native `/`
    // slash commands trigger Slack's autocomplete and don't fire in
    // threads, whereas `cmd:` is parsed everywhere (a plain `/` still
    // works outside threads).
    meta_lines.push("- Commands (tell users to prefix with `cmd:` — works in Slack threads too): `cmd: new`, `cmd: reset`, `cmd: status`, `cmd: compact`, `cmd: context`, `cmd: model`, `cmd: health`, `cmd: wake`, `cmd: heartbeat`, `cmd: help`".to_string());

    turn_context.push_str("\n\n");
    turn_context.push_str(&meta_lines.join("\n"));

    // ── Voice Mode Directive ──────────────────────────────────────────
    // When this turn arrived over voice, inject formatting rules so the
    // agent responds conversationally for TTS playback.
    //
    // Per turn, not per session (DW-571): the session-level flag latches
    // on for the life of an iOS session, so keying on it left the agent
    // answering typed questions as if they were going to be read aloud —
    // short, chatty, unformatted — long after the user stopped speaking.
    // `tts_provider` stays session-level; it is the engine, not the mode.
    if let Some(session) = session_id.and_then(|sid| service.sessions.get(sid).ok().flatten()) {
        if session.is_voice_turn() {
            let provider = session
                .metadata
                .get("tts_provider")
                .and_then(|v| v.as_str())
                .unwrap_or("system");
            turn_context.push_str("\n\n");
            turn_context.push_str(&voice_mode_directive(provider, &session.channel));
        }
    }

    TurnPrompt {
        stable,
        turn_context,
        injected_lessons,
        lesson_events,
    }
}

/// Session-bound wrapper around [`fuse_recall_query`]. Returns `None`
/// when no session is in scope or it can't be loaded; otherwise delegates
/// to the pure helper with the session's message tail.
pub(crate) fn build_fused_recall_query(
    service: &AgentService,
    session_id: Option<&str>,
    user_message: &str,
) -> Option<String> {
    let session = service.sessions.get(session_id?).ok().flatten()?;
    fuse_recall_query(&session.messages, user_message)
}

/// Build a compact summary of other active sessions for cross-session awareness.
///
/// Scope is the current session's owner — sessions belonging to other users are
/// never surfaced. If the current session can't be loaded the section is suppressed.
/// So is an automated run's: its session is owned by its source, as is every
/// other run's, whoever each one ran as.
pub(crate) fn build_cross_session_summary(
    service: &AgentService,
    current_session_id: &str,
) -> String {
    let Ok(Some(current)) = service.sessions.get(current_session_id) else {
        return String::new();
    };
    if !is_person(&current.owner) {
        return String::new();
    }

    let sessions = match service.sessions.list(None, 10) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };

    let cutoff = chrono::Utc::now() - chrono::Duration::hours(4);
    let other_active =
        select_visible_other_sessions(&sessions, current_session_id, &current.owner, cutoff);

    if other_active.is_empty() {
        return String::new();
    }

    let mgr = crate::memory::MemoryManager::new(&service.workspace);

    let mut lines = vec![
        "## Other Active Sessions".to_string(),
        "You are also active in these sessions. Do NOT resume or work on their tasks here — they are handled independently. Use `list_all_tasks` for a full overview.".to_string(),
        String::new(),
    ];

    for session in &other_active {
        let channel_label = session.label.as_deref().unwrap_or(&session.channel);

        let age = chrono::Utc::now() - session.updated_at;
        let age_str = if age.num_minutes() < 60 {
            format!("{}m ago", age.num_minutes())
        } else {
            format!("{}h ago", age.num_hours())
        };

        // Try to load per-session tasks for this session
        let task_summary = match mgr.load_session_tasks(&session.id) {
            Ok(tasks) => {
                let active: Vec<_> = tasks
                    .iter()
                    .filter(|t| t.status == crate::memory::SessionTaskStatus::Active)
                    .collect();
                if active.is_empty() {
                    String::new()
                } else {
                    let descs: Vec<&str> = active
                        .iter()
                        .take(3)
                        .map(|t| t.description.as_str())
                        .collect();
                    let suffix = if active.len() > 3 {
                        format!(" (+{} more)", active.len() - 3)
                    } else {
                        String::new()
                    };
                    format!(" — Tasks: {}{}", descs.join(", "), suffix)
                }
            }
            Err(_) => String::new(),
        };

        let thread_link = session
            .metadata
            .get("slack_thread_url")
            .and_then(|v| v.as_str())
            .map(|url| format!(" — [Thread]({})", url))
            .unwrap_or_default();

        lines.push(format!(
            "- **{}** (last active {}){}{}",
            channel_label, age_str, thread_link, task_summary
        ));
    }

    lines.join("\n")
}

/// Build the replay policy for a shared session, or `None` when the session
/// is single-owner (DM/web/internal). Resolves a display label per
/// participant present in the transcript so prior turns can be attributed.
pub(crate) fn shared_replay(
    service: &AgentService,
    session: &Session,
    viewer: &str,
) -> Option<SharedReplay> {
    if !session.is_shared_conversation() {
        return None;
    }
    let mut display = std::collections::HashMap::new();
    for actor in session.messages.iter().filter_map(|m| m.actor.as_deref()) {
        display
            .entry(actor.to_string())
            .or_insert_with(|| resolve_actor_display(service, actor));
    }
    Some(SharedReplay {
        viewer: viewer.to_string(),
        display,
    })
}

/// A human-readable label for an actor id, preferring the configured roster
/// (keyed by platform id) and falling back through the actor's platform id to
/// the raw id, so attribution degrades gracefully when no name is known.
pub(crate) fn resolve_actor_display(service: &AgentService, actor: &str) -> String {
    if let Some(name) = service.user_map.get(actor) {
        return name.clone();
    }
    let platform = service.resolve_platform_user_id(actor);
    if !platform.is_empty() {
        if let Some(name) = service.user_map.get(&platform) {
            return name.clone();
        }
    }
    actor.to_string()
}

/// Deterministic failed-tool notice (G2): flags tool families whose LAST
/// invocation this turn failed with no later success, so the reader knows
/// which claims may rest on actions that never happened. No LLM involved.
/// Gated by `claim_grounding.flag_failed_tools` (default off), read
/// directly — independent of `claim_grounding.enabled`.
pub(crate) fn failed_tools_notice(
    service: &AgentService,
    invocations: &[ToolInvocation],
) -> Option<String> {
    if !service
        .resolved
        .snapshot()
        .claim_grounding
        .flag_failed_tools
    {
        return None;
    }
    let failures = crate::failed_tools::unrecovered_failures(invocations);
    let notice = crate::failed_tools::failed_tools_block(&failures)?;
    let tools: Vec<&str> = failures.iter().map(|f| f.tool.as_str()).collect();
    tracing::info!(target: "failed_tools", agent = %service.agent_id, n_failed = tools.len(), ?tools, "failed tools notice");
    Some(notice)
}

/// Fact-check a code-shaped answer's claims against the project source and
/// report any the source contradicts. Not run when the feature is off, the
/// response is short or not about code, the answer was written from
/// injected source, or no source-read tools exist. Best-effort — a failed
/// or cut-short pass reports nothing.
pub(crate) async fn claim_grounding_block(
    service: &Arc<AgentService>,
    request: &str,
    answer: &str,
    injected_source: &str,
    invocations: &[ToolInvocation],
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> GroundingOutcome {
    if !claim_grounding_enabled(service) {
        return GroundingOutcome::NotRun;
    }
    if !injected_source.trim().is_empty() {
        // The answer was generated FROM system-assembled, HEAD-read source,
        // and the deterministic citations carry that provenance to the
        // reader. Re-checking the answer against the very block it was
        // written from is a tautology, not verification — and the residual
        // (absence claims) is the ANSWER's obligation now: the injection
        // header requires a search beyond the block before asserting that
        // something does not exist.
        tracing::info!(target: "claim_grounding", agent = %service.agent_id, "skipped: answer grounded by injected source; citations carry the confirmation");
        return GroundingOutcome::NotRun;
    }
    if answer.len() < CLAIM_GROUNDING_MIN_RESPONSE {
        tracing::info!(target: "claim_grounding", agent = %service.agent_id, response_len = answer.len(), "skipped: response below min length");
        return GroundingOutcome::NotRun;
    }
    if !answer_is_code_shaped(answer, invocations) {
        tracing::info!(target: "claim_grounding", agent = %service.agent_id, "skipped: answer is not about code");
        return GroundingOutcome::NotRun;
    }
    let Some(tools) = service.source_review_tools() else {
        tracing::info!(target: "claim_grounding", agent = %service.agent_id, "skipped: no source-read tools available");
        return GroundingOutcome::NotRun;
    };
    tracing::info!(target: "claim_grounding", agent = %service.agent_id, request_len = request.len(), response_len = answer.len(), n_tools = tools.len(), "running");
    let grounder = crate::claim_verifier::ClaimGrounder::new(Arc::new(with_cancel_opt(
        crate::claim_verifier::AdvisorClaimChecker::new(
            grounding_loop(service).await,
            tools,
            service.resolved.snapshot().claim_grounding.model.clone(),
        )
        .with_injected_source(injected_source.to_string()),
        cancel,
        crate::claim_verifier::AdvisorClaimChecker::with_cancel,
    )));
    // The budget is the real bound: it stops the research early enough that
    // the write-up keeps its reserve, and bounds the write-up separately. The
    // outer timeout is only a backstop for a provider that hangs through
    // both, so it sits past their sum rather than cutting into it — which is
    // what previously killed a write-up mid-sentence and threw away every
    // round of research behind it.
    let (review_total, reserve) = service.resolved.snapshot().claim_grounding.review_budget();
    let budget = crate::advisor_loop::LoopBudget {
        research_deadline: std::time::Instant::now() + (review_total - reserve),
        write_up_reserve: reserve,
    };
    let grounding = tokio::time::timeout(
        review_total + REPLY_REVIEW_BACKSTOP_SLACK,
        grounder.ground_by(request, answer, budget),
    )
    .await;
    // A turn the reader stopped is not a failed check — they are not waiting
    // for a verdict, so it reports nothing.
    if cancel.is_some_and(|c| c.is_cancelled()) {
        return GroundingOutcome::Cancelled;
    }
    match grounding {
        Ok(Ok(report)) => {
            let outcome = GroundingOutcome::of_report(&report);
            tracing::info!(target: "claim_grounding", agent = %service.agent_id, n_claims = report.claims.len(), n_contradicted = report.tier_count(crate::claim_verifier::Tier::Contradicted), flagged = matches!(outcome, GroundingOutcome::Flagged(_)), outcome = outcome.label(), complete = report.complete, "claim grounding complete");
            outcome
        }
        Ok(Err(e)) => {
            tracing::warn!(target: "claim_grounding", agent = %service.agent_id, error = %e, "claim grounding failed");
            GroundingOutcome::Incomplete
        }
        Err(_) => {
            tracing::warn!(target: "claim_grounding", agent = %service.agent_id, timeout_secs = review_total.as_secs(), "claim grounding timed out");
            GroundingOutcome::Incomplete
        }
    }
}

/// Read-only probe tools for the data-claim verifier: `data.query` over
/// staged artifacts and `tool.retrieve` for spilled outputs. May be empty —
/// the evidence digest alone still grounds. No upstream re-query tools by
/// construction.
pub(crate) fn data_review_tools(
    service: &AgentService,
) -> Vec<Arc<dyn workforce_agent::tools::Tool>> {
    ["data.query", "tool.retrieve"]
        .iter()
        .filter_map(|n| service.tools.get(n))
        .collect()
}

/// Fact-check the answer's DATA claims (numbers/counts/names/dates
/// attributed to tool-fetched data) against this turn's own tool outputs
/// and staged artifacts, and return a "verify before acting" block for any
/// they contradict or do not contain — the evidence set is closed, so a
/// figure no tool produced is itself the finding. Gated by `claim_grounding.data_claims`
/// (read directly, independent of `enabled` — the code lens). The
/// task-local scoping is required: `data.query`/`tool.retrieve` resolve
/// store + agent + session from `CURRENT_AGENT_ID`/`CURRENT_SESSION_ID`
/// (and artifact ownership from `CURRENT_USER_ID`), which are no longer in
/// scope at the delivery sites. Best-effort — any failure returns `None`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn data_claim_grounding_block(
    service: &Arc<AgentService>,
    request: &str,
    answer: &str,
    session_id: &str,
    user_id: &str,
    invocations: &[ToolInvocation],
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Option<String> {
    let snapshot = service.resolved.snapshot();
    if let Some(reason) = data_claim_grounding_skip(
        snapshot.claim_grounding.data_claims,
        answer.len(),
        invocations.len(),
    ) {
        if reason != "disabled" {
            tracing::info!(target: "data_claim_grounding", agent = %service.agent_id, response_len = answer.len(), n_invocations = invocations.len(), "skipped: {reason}");
        }
        return None;
    }
    let tools = data_review_tools(service);
    tracing::info!(target: "data_claim_grounding", agent = %service.agent_id, request_len = request.len(), response_len = answer.len(), n_invocations = invocations.len(), n_tools = tools.len(), "running");
    let evidence = crate::claim_verifier::tool_evidence_digest(invocations);
    let grounder = crate::claim_verifier::ClaimGrounder::new(Arc::new(with_cancel_opt(
        crate::claim_verifier::DataClaimChecker::new(
            grounding_loop(service).await,
            tools,
            snapshot.claim_grounding.model.clone(),
            evidence,
        ),
        cancel,
        crate::claim_verifier::DataClaimChecker::with_cancel,
    )));
    let grounding = tokio::time::timeout(
        DATA_CLAIM_REVIEW_TIMEOUT,
        CURRENT_AGENT_ID.scope(
            service.agent_id.as_str().to_string(),
            CURRENT_SESSION_ID.scope(
                session_id.to_string(),
                CURRENT_USER_ID.scope(user_id.to_string(), grounder.ground(request, answer)),
            ),
        ),
    )
    .await;
    match grounding {
        Ok(Ok(report)) => {
            let block = report.render_unsupported("this turn's data");
            tracing::info!(target: "data_claim_grounding", agent = %service.agent_id, n_claims = report.claims.len(), n_contradicted = report.tier_count(crate::claim_verifier::Tier::Contradicted), flagged = block.is_some(), "data claim grounding complete");
            block
        }
        Ok(Err(e)) => {
            tracing::warn!(target: "data_claim_grounding", agent = %service.agent_id, error = %e, "data claim grounding failed");
            None
        }
        Err(_) => {
            tracing::warn!(target: "data_claim_grounding", agent = %service.agent_id, timeout_secs = DATA_CLAIM_REVIEW_TIMEOUT.as_secs(), "data claim grounding timed out");
            None
        }
    }
}

/// Post-turn commitment check (P1): when the final text makes an
/// UNCONDITIONAL future commitment and no scheduling-class tool call
/// succeeded this turn, return a visible caveat block. The deterministic
/// backing-call short-circuit runs first — the common case never pays an
/// LLM call. Gated by `claim_grounding.commitments` (default off).
/// Fail-safe in every branch: absence of the block never blocks delivery.
pub(crate) async fn commitment_caveat_block(
    service: &AgentService,
    answer: &str,
    invocations: &[ToolInvocation],
    origin: Origin,
) -> Option<String> {
    let snapshot = service.resolved.snapshot();
    if !snapshot.claim_grounding.commitments {
        return None;
    }
    if answer.trim().is_empty() {
        return None;
    }
    if crate::commitments::has_backing_call(invocations) {
        return None;
    }
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(COMMITMENT_CHECK_TIMEOUT_SECS),
        with_served_log(with_reasoning_suppressed(service.llm.create_message(
            Some(crate::commitments::COMMITMENT_SYSTEM),
            &[LlmMessage::user(
                crate::commitments::answer_tail(answer).to_string(),
            )],
            &[],
            snapshot.claim_grounding.model.as_deref(),
            Some(300),
            true,
        ))),
    )
    .await;
    match outcome {
        Ok((Ok(response), served)) => {
            let billed_to = crate::user_config::current_billing_identity_id();
            service.cost_tracker.record(CostEntry {
                task_type: Some("commitment_check"),
                identity_id: billed_to.as_deref(),
                description: Some("claim_grounding:commitments"),
                served,
                ..CostEntry::new(
                    service.agent_id.as_str(),
                    &response.model,
                    TokenCounts::from(&response.usage),
                    origin,
                )
            });
            crate::commitments::parse_commitment(&response.all_text().unwrap_or_default())
                .map(|c| crate::commitments::caveat_block(&c))
        }
        Ok((Err(e), _)) => {
            tracing::warn!(target: "commitments", agent = %service.agent_id, error = %e, "commitment classifier failed");
            None
        }
        Err(_) => {
            tracing::warn!(target: "commitments", agent = %service.agent_id, timeout_secs = COMMITMENT_CHECK_TIMEOUT_SECS, "commitment classifier timed out");
            None
        }
    }
}

/// What a run in the session `session_key` says of itself in its trace
/// header. Nothing without a trace to write it to. What was asked is
/// kept only while this agent's decisions are recorded: it is there for
/// the same export, under the same switches.
pub(crate) fn trace_tags(
    service: &AgentService,
    session_key: &str,
    request: Option<&str>,
    task_type: Option<&str>,
) -> workforce_agent::trace::TraceTags {
    if service.trace_writer.is_none() {
        return Default::default();
    }
    let tags = crate::serve::trace_tags(session_key, request, task_type, |instance| {
        let run = service.telemetry_store.get_workflow_instance(instance);
        run.ok().flatten().map(|run| run.workflow_id)
    });
    let recorded = service.config.decision.enabled && service.decision_records().enabled;
    without_request_unless(recorded, tags)
}

/// Who a turn's know-how is for: a linked person in a conversation, never
/// a heartbeat, schedule or other internal session.
pub(crate) fn know_how_person(
    _service: &AgentService,
    session: &workforce_common::session::Session,
    principal: &str,
) -> Option<String> {
    if crate::turn_extraction::is_internal_channel(&session.channel) {
        return None;
    }
    crate::endpoint_cmd::identity_id(principal)
}

/// The know-how block to put in front of this request (`inject = "on"`);
/// under `shadow` the matches are only recorded. Only a turn that is the
/// person's `own` is shown what they alone have learned.
pub(crate) async fn know_how_block(
    service: &AgentService,
    session: &workforce_common::session::Session,
    principal: &str,
    question: &str,
    own: bool,
) -> Option<String> {
    let cfg = service.know_how()?;
    let person = know_how_person(service, session, principal)?;
    let kh = crate::know_how::KnowHow::shared(&service.config).await;
    let access = crate::know_how::agent::access(service, Some(session), &person).await;
    let agent = service.agent_id.as_str();
    let shared = !own;
    match crate::turn_extraction::is_eval_channel(&session.channel) {
        true => {
            kh.for_eval(agent, &cfg, &person, question, &access, shared)
                .await
        }
        false => {
            kh.for_request(agent, &session.id, &cfg, &person, question, &access, shared)
                .await
        }
    }
}

/// Learn from a finished turn in the background (see [`crate::know_how`]).
pub(crate) async fn spawn_know_how(
    service: &AgentService,
    session: &workforce_common::session::Session,
    principal: &str,
    question: &str,
    invocations: &[workforce_agent::runner::ToolInvocation],
) {
    let Some(cfg) = service.know_how() else {
        return;
    };
    let Some(person) = know_how_person(service, session, principal) else {
        return;
    };
    if crate::turn_extraction::is_eval_channel(&session.channel) {
        return;
    }
    let kh = crate::know_how::KnowHow::shared(&service.config).await;
    let matched = kh.take_matched(&session.id);
    let agent = service.agent_id.to_string();
    let invocations = invocations.to_vec();
    if !crate::know_how::teaches(&invocations) {
        if let Some(matched) = matched {
            workforce_agent::runner::spawn_in_session_context(async move {
                kh.settle(&agent, matched, &invocations).await;
            });
        }
        return;
    }
    let sites = crate::know_how::agent::sites(service, Some(session), &person).await;
    let shared = session.is_shared_conversation();
    let question = question.to_string();
    let llm = Arc::clone(&service.llm);
    let cost_tracker = Arc::clone(&service.cost_tracker);
    let model = cheap_model(service);
    let billed_to = person.clone();
    let cost_agent = agent.clone();
    let write_intent: crate::know_how::IntentWriter = Box::new(move |q: String| {
        Box::pin(async move {
            let (resp, served) = with_served_log(llm.create_message(
                Some(crate::know_how::INTENT_PROMPT),
                &[workforce_agent::llm::types::LlmMessage::user(format!(
                    "Request:\n{q}"
                ))],
                &[],
                Some(&model),
                Some(60),
                true,
            ))
            .await;
            let resp = resp.ok()?;
            cost_tracker.record(CostEntry {
                task_type: Some("know_how_intent"),
                identity_id: Some(billed_to.as_str()),
                description: Some("know-how:intent"),
                served,
                ..CostEntry::new(
                    &cost_agent,
                    &model,
                    TokenCounts::from(&resp.usage),
                    Origin::Knowledge,
                )
            });
            Some(workforce_agent::compaction::extract_text_from_response(
                &resp.content,
            ))
        })
    });
    workforce_agent::runner::spawn_in_session_context(async move {
        kh.observe(
            &cfg,
            crate::know_how::Turn {
                agent: &agent,
                identity: &person,
                shared,
                question: &question,
                invocations: &invocations,
                sites,
            },
            matched,
            write_intent,
        )
        .await;
    });
}

/// Keep what a turn's prompt surfaced of the agent's lessons: the ids on
/// the session for the post-turn reinforcement, and one event each for
/// the lesson statistics. An eval's turn keeps neither: its lessons were
/// surfaced by a set's question, not by a request anyone made.
pub(crate) fn note_surfaced_lessons(
    service: &AgentService,
    session: &mut workforce_common::session::Session,
    prompt: &TurnPrompt,
) {
    if crate::turn_extraction::is_eval_channel(&session.channel) {
        return;
    }
    stash_injected_lessons(session, &prompt.injected_lessons);
    for event in &prompt.lesson_events {
        let _ = service.telemetry_store.record_lesson_event(event);
    }
}

pub(crate) fn spawn_turn_extraction(
    service: &AgentService,
    session: &workforce_common::session::Session,
    archive_base: usize,
    effective_message: &str,
    response: &str,
    principal: &str,
) {
    if crate::turn_extraction::is_eval_channel(&session.channel) {
        return;
    }
    let mem = service.effective_memory();
    let turn_tokens = estimate_tokens(effective_message) + estimate_tokens(response);
    let is_dm = session.context_type == Some(workforce_common::session::SessionContextType::Dm);
    let (want_passive, want_relationship) = plan_turn_extraction(
        &mem,
        turn_tokens,
        is_dm,
        session.is_shared_conversation(),
        service.memory_store.is_some(),
        workforce_agent::runner::game_sandboxed(),
        principal,
    );
    if !(want_passive || want_relationship) {
        return;
    }

    // Slice the turn from `archive_base` (pre-turn length) and skip tool
    // entries so tool-chatter doesn't end up in the memory excerpt.
    let turn_messages: Vec<&workforce_common::session::SessionMessage> = session
        .messages
        .get(archive_base..)
        .unwrap_or_default()
        .iter()
        .filter(|m| !matches!(m.role, workforce_common::session::MessageRole::Tool))
        .collect();
    let turn_message_ids: Vec<String> = turn_messages.iter().map(|m| m.id.clone()).collect();
    let pm_event_time = turn_messages.iter().map(|m| m.timestamp).max();
    let user_turn = turn_messages
        .iter()
        .find(|m| matches!(m.role, workforce_common::session::MessageRole::User))
        .map(|m| (m.id.clone(), m.timestamp));
    let pm_llm = Arc::clone(&service.llm);
    let pm_store = if want_passive {
        service.memory_store.as_ref().map(Arc::clone)
    } else {
        None
    };
    let pm_agent_id = service.agent_id.clone();
    let pm_channel = session.channel.clone();
    let pm_session_id = session.id.clone();
    let pm_user_msg = effective_message.to_string();
    let pm_agent_resp = response.to_string();
    let pm_model = mem
        .passive_model
        .clone()
        .or_else(|| service.current_compaction_config().compaction_model.clone());
    let pm_prompt = mem.passive_prompt.clone();
    let pm_cost_tracker = Arc::clone(&service.cost_tracker);
    let rel_identity = principal.to_string();
    let rel_telemetry = Arc::clone(&service.telemetry_store);
    spawn_in_session_context(async move {
        if let Some(store) = pm_store {
            if let Err(e) = crate::passive_memory::extract_and_store(
                pm_llm,
                store,
                pm_agent_id.clone(),
                &pm_channel,
                Some(pm_session_id.clone()),
                turn_message_ids,
                pm_event_time,
                &pm_user_msg,
                &pm_agent_resp,
                pm_model,
                pm_prompt,
                Some(pm_cost_tracker),
            )
            .await
            {
                tracing::warn!(error = %e, "Passive memory extraction failed");
            }
        }

        if want_relationship {
            if let Some((msg_id, msg_ts)) = user_turn {
                if let Err(e) = crate::relationship::capture_turn(
                    rel_telemetry.as_ref(),
                    pm_agent_id.as_str(),
                    &rel_identity,
                    &pm_session_id,
                    &msg_id,
                    msg_ts,
                    &pm_user_msg,
                ) {
                    tracing::warn!(error = %e, "Relationship capture failed");
                }
            }
        }
    });
}

/// One admission for a prospective LLM turn: nothing runs as a suspended
/// identity, then the budget. `identity` must be the same principal the
/// runner will scope `CURRENT_USER_ID` to, so the scope that is checked is
/// exactly the scope the spend will be recorded against. `Some(text)`
/// means the turn is denied and `text` is the user-facing reply; a soft
/// warning is delivered best-effort to `channel_id` (once per scope per
/// day, deduplicated by the governor).
pub(crate) async fn admit_turn_or_deny(
    service: &AgentService,
    identity: Option<&str>,
    channel_id: Option<&str>,
) -> Option<String> {
    if let Some(person) = identity.and_then(|principal| service.suspended(principal)) {
        tracing::warn!(agent = %service.agent_id, person = %person.id, "Turn refused: the person it runs as is suspended");
        return Some(crate::hub_core::standing::refusal(&person));
    }
    let identity = crate::user_config::current_billing_identity_id()
        .or_else(|| identity.map(str::to_string).filter(|s| !s.is_empty()));
    match service
        .cost_governor
        .admit_turn(service.agent_id.as_str(), identity.as_deref())
    {
        crate::cost_governor::Admission::Proceed => None,
        crate::cost_governor::Admission::Warn(notice) => {
            tracing::warn!(agent = %service.agent_id, %notice, "LLM budget soft warning");
            if let Some(cid) = channel_id {
                let registry = service.surface_registry.read().await;
                for surface in registry.all() {
                    let _ = surface
                        .send_message(&workforce_common::channels::OutboundMessage::text(
                            cid,
                            format!("⚠️ {notice}"),
                        ))
                        .await;
                }
            }
            None
        }
        crate::cost_governor::Admission::Deny(reason) => {
            tracing::warn!(agent = %service.agent_id, %reason, "LLM turn denied by budget");
            Some(reason)
        }
    }
}

/// Process an incoming message and return the response.
///
/// Slash commands (e.g. `/status`, `/new`) are intercepted and handled
/// before reaching the LLM. A turn the budget refuses returns the
/// refusal; automated callers that must tell the two apart use
/// [`Self::process_turn`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_message(
    service: &Arc<AgentService>,
    channel: &str,
    user: &str,
    message: &str,
    external_id: Option<&str>,
    channel_system_prompt: Option<&str>,
    channel_id: Option<&str>,
    thread_id: Option<&str>,
    attachments: Option<Vec<SessionAttachment>>,
    max_rounds_override: Option<u32>,
    model_override: Option<&str>,
    task_id: Option<&str>,
    trust_level: Option<workforce_common::trust::TrustLevel>,
    run_as: Option<&str>,
) -> Result<String> {
    process_turn(
        service,
        channel,
        user,
        message,
        external_id,
        channel_system_prompt,
        channel_id,
        thread_id,
        attachments,
        max_rounds_override,
        model_override,
        task_id,
        trust_level,
        run_as,
        RunAuthor::Other,
    )
    .await
    .map(TurnOutcome::into_reply)
}

/// [`Self::process_message`], saying whether the turn ran or the budget
/// refused it ([`Skip::Denied`]), for automated callers, which also say
/// who is in charge of the turn (`author`, read by [`prompt_privacy`]).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_turn(
    service: &Arc<AgentService>,
    channel: &str,
    user: &str,
    message: &str,
    external_id: Option<&str>,
    channel_system_prompt: Option<&str>,
    channel_id: Option<&str>,
    thread_id: Option<&str>,
    attachments: Option<Vec<SessionAttachment>>,
    max_rounds_override: Option<u32>,
    model_override: Option<&str>,
    task_id: Option<&str>,
    trust_level: Option<workforce_common::trust::TrustLevel>,
    run_as: Option<&str>,
    author: RunAuthor,
) -> Result<TurnOutcome> {
    // Check for voice transcript messages and handle them specially
    if message.starts_with("[VOICE_TRANSCRIPT]")
        || message.starts_with("[VOICE_SESSION_START]")
        || message.starts_with("[VOICE_SESSION_END]")
    {
        // This is a voice transcript log message - write directly to daily notes
        let mgr = crate::memory::MemoryManager::for_user(&service.workspace, user);

        // Extract the actual transcript content
        let log_entry = if message.starts_with("[VOICE_TRANSCRIPT]") {
            // Parse: [VOICE_TRANSCRIPT] <timestamp> - <text>
            if let Some(content_start) = message.find(" - ") {
                let timestamp_part = &message["[VOICE_TRANSCRIPT] ".len()..content_start];
                let transcript_part = &message[content_start + 3..];
                format!("Audio: {} - {}", timestamp_part, transcript_part)
            } else {
                message.to_string()
            }
        } else if message.starts_with("[VOICE_SESSION_START]") {
            // Parse session start
            let timestamp = chrono::Utc::now()
                .format("%Y-%m-%d %H:%M:%S UTC")
                .to_string();
            format!("\n## Audio Session Started\nTime: {}\n", timestamp)
        } else {
            // Parse session end
            let duration_info = if let Some(dur_start) = message.find("duration: ") {
                let dur_end = message[dur_start..]
                    .find(char::is_whitespace)
                    .unwrap_or(message.len() - dur_start);
                &message[dur_start..dur_start + dur_end]
            } else {
                "duration: unknown"
            };
            let timestamp = chrono::Utc::now()
                .format("%Y-%m-%d %H:%M:%S UTC")
                .to_string();
            format!("Session ended: {} ({})\n", timestamp, duration_info)
        };

        // Write to daily notes
        if let Err(e) = mgr.append_daily(&log_entry) {
            tracing::warn!(error = %e, "Failed to write voice transcript to daily notes");
        }

        // On voice session end, extract a conversation summary and write it
        // to daily notes so the substance of the chat is preserved (DW-439/441).
        if message.starts_with("[VOICE_SESSION_END]") {
            let service = Arc::clone(service);
            let channel_owned = channel.to_string();
            let user_owned = user.to_string();
            let summary_scope = SessionContext {
                user_id: Some(run_as_principal(run_as, user).to_string()),
                agent_id: Some(service.agent_id.as_str().to_string()),
                ..SessionContext::capture()
            };
            summary_scope.spawn(async move {
                if let Err(e) =
                    write_voice_session_summary(&service, &channel_owned, &user_owned).await
                {
                    tracing::warn!(
                        error = %e,
                        channel = %channel_owned,
                        "Failed to write voice session summary"
                    );
                }
            });
        }

        // Return early - do not process these as normal messages
        return Ok(TurnOutcome::Answered(
            "Voice transcript logged.".to_string(),
        ));
    }
    // Slash commands stay free of the budget, not of a suspension: they
    // run workflows, write secrets and steer monitors as the person.
    let principal = run_as_principal(run_as, user);
    if let Some(person) = service.suspended(principal) {
        return Ok(TurnOutcome::Skipped(Skip::Denied(
            crate::hub_core::standing::refusal(&person),
        )));
    }
    if let Some(cmd) = crate::commands::parse_command(message) {
        let result = crate::commands::execute_command(&cmd, service, channel, user).await;
        return Ok(TurnOutcome::Answered(result.text));
    }

    // Mandatory budget admission — the single choke point for every LLM
    // turn (chat, scheduler jobs, heartbeats, monitors, consolidation).
    if let Some(denied) = admit_turn_or_deny(service, Some(principal), channel_id).await {
        return Ok(TurnOutcome::Skipped(Skip::Denied(denied)));
    }

    // Everything after admission runs as the principal, so the post-run
    // tail (grounding, extraction, lessons, background compaction) and
    // its spawns see who the turn was for.
    SessionContext {
        user_id: Some(principal.to_string()),
        agent_id: Some(service.agent_id.as_str().to_string()),
        ..SessionContext::capture()
    }
    .scope(Box::pin(process_admitted(
        service,
        AdmittedTurn {
            channel,
            user,
            principal,
            message,
            external_id,
            channel_system_prompt,
            channel_id,
            thread_id,
            attachments,
            max_rounds_override,
            model_override,
            task_id,
            trust_level,
            author,
        },
    )))
    .await
}

/// The body of [`Self::process_message`] once the turn is admitted.
pub(crate) async fn process_admitted(
    service: &Arc<AgentService>,
    turn: AdmittedTurn<'_>,
) -> Result<TurnOutcome> {
    let AdmittedTurn {
        channel,
        user,
        principal,
        message,
        external_id,
        channel_system_prompt,
        channel_id,
        thread_id,
        attachments,
        max_rounds_override,
        model_override,
        task_id,
        trust_level,
        author,
    } = turn;

    // Defence-in-depth tokenization. No-op when the surface layer
    // already tokenized; catches any caller that didn't.
    let tokenized = workforce_common::redact::tokenize_secrets(
        message,
        channel,
        workforce_common::secret_marker::SecretSource::Unknown,
    )
    .0;
    let message: &str = &tokenized;

    // Get or create session. A shared room keys by thread (not by user) so
    // every participant continues one conversation.
    let owner = session_owner_for_turn(channel, channel_id, thread_id, external_id, user);
    let session = service
        .sessions
        .get_or_create(channel, &owner, service.agent_id.as_str())?;
    let session = service.roll_stale_for_resume(channel, session)?;

    // One live turn per session across the fleet — held to the end of this
    // fn (all exit paths) by the RAII guard. Refusal is the typed
    // `TurnClaimHeld` so callers can defer or render an honest wait.
    let (mut session, _turn_claim) = claim_for_turn(service, session)?;

    // Classify the session's context once (DM vs shared channel/thread) so
    // per-type reset policies and relationship tracking (DM-only) have a
    // persisted signal. Idempotent: only the first turn sets it.
    if session.context_type.is_none() {
        session.context_type = Some(classify_context_type(channel_id, thread_id));
    }

    // Build augmented message with channel context and welcome directive.
    // The original message is stored in the session; augmentation only affects the LLM.
    let is_new_session = session.messages.is_empty();
    let mut effective_message = message.to_string();

    // Inject messaging context so the agent knows channel and thread for tool calls
    let current_session_id = session.id.clone();
    if let Some(block) = surface::surface_context(
        &surface::TurnSurface::of(channel_id, thread_id, None),
        channel,
        &current_session_id,
        user,
        None,
    ) {
        effective_message = format!("{block}\n\n{effective_message}");
    }

    // Prepend channel-specific system prompt if configured
    if let Some(prompt) = channel_system_prompt {
        effective_message = format!("[CHANNEL CONTEXT]\n{}\n\n{}", prompt, effective_message);
    }

    // On the first message of a new *conversational* session, prepend the
    // welcome directive. Automated background turns (scheduler jobs, monitor
    // tasks) run in a fresh session every fire but are NOT conversational
    // first-contacts — a "incorporate a brief greeting" directive is wrong
    // for them and, worse, reads as injection scaffolding wrapped around the
    // task (the agent quoted this very "[WELCOME DIRECTIVE …]" envelope as
    // the thing it was refusing). So skip it for those sources.
    if is_new_session && !is_automated_source(user) {
        if let Some(welcome) = service.welcome().await {
            effective_message = format!(
                "[WELCOME DIRECTIVE — first message in this channel, incorporate a brief greeting]\n{}\n\n[User's message]\n{}",
                welcome, effective_message
            );
        }
    }

    let privacy = prompt_privacy(
        user,
        principal,
        prompt_visibility(&session, channel_id),
        author,
    );

    // Cross-channel continuity: a conversational session opening with no
    // dialogue yet gets a dated digest of the user's own recent memories.
    if let Some(block) =
        resume_context_block(service, &session, privacy == PromptPrivacy::Private).await
    {
        effective_message = format!("{block}\n\n{effective_message}");
    }

    // Same-agent continuity: a 1:1 DM session opening with no dialogue
    // yet gets a digest of this user's other recent sessions.
    if let Some(block) = recent_sessions_digest_block(service, &session, user, channel_id) {
        effective_message = format!("{block}\n\n{effective_message}");
    }

    if let Some(block) = named_secret_context(channel) {
        effective_message = format!("{block}\n\n{effective_message}");
    }

    // An agent that runs programs is told, in the request itself, when to
    // answer with one: guidance a model reads as part of the ask.
    if let Some(hint) = service.programs().as_deref().and_then(|p| p.hint()) {
        effective_message = format!("{hint}\n\n{effective_message}");
    }

    // What the agent has learned about where data lives, for requests
    // like this one (`[know_how] inject = "on"`; `shadow` only records).
    if let Some(block) = know_how_block(
        service,
        &session,
        principal,
        message,
        privacy == PromptPrivacy::Private,
    )
    .await
    {
        effective_message = format!("{block}\n{effective_message}");
    }

    // Answer-ready code context: before the model's first turn, assemble the
    // complete current source for the user's question and prepend it as an
    // authoritative block so the model answers from real code instead of
    // guessing. Gated on `[code_context] enabled`, the session not being a
    // voice conversation, and the assembler's own relevance self-gate
    // (non-code questions assemble nothing → no injection). The raw
    // `message` is the question; the block lands in front of
    // `effective_message`, which `build_task_content` wraps as the model's
    // first user turn.
    // Kept apart for the review passes: folded together, the assembled source
    // reaches them labelled "ORIGINAL QUESTION" rather than as the evidence
    // it is.
    let question_only = effective_message.clone();
    let mut injected_source = String::new();
    let mut injected_block: Option<String> = None;
    if let Some(ctx) = assemble_code_context(service, message, &session).await {
        injected_source = ctx.text.clone();
        // System-assembled source travels as its OWN content block (composed
        // in `build_task_content`, outside the trust wrap) — but through the
        // same secret tokenizer as every other model-bound surface first: a
        // credential committed to the repo must not reach the model raw just
        // because the system, not the sender, read it.
        injected_block = Some(
            workforce_common::redact::tokenize_secrets(
                &crate::code_context::build_injected_block(&ctx.text),
                channel,
                workforce_common::secret_marker::SecretSource::Unknown,
            )
            .0,
        );
    }

    let shared = session.is_shared_conversation();
    let (turn_journal, collect) = crate::turn_journal::SessionJournal::open(
        Arc::clone(&service.sessions),
        &mut session,
        shared.then_some(user),
    );
    let archive_base = session.messages.len();

    session.add_user_message_with_trust(message, external_id, attachments.clone(), trust_level);

    stamp_tool_whitelist(&mut session);
    let turn_tools = crate::turn_loop::tools::turn_tools(service, &session, None).await;
    let turn_prompt = build_enhanced_prompt_with_context(
        service,
        Some(&current_session_id),
        Some(principal),
        is_new_session,
        Some(message),
        None,
        privacy,
        is_workflow_step_session(&session),
        PromptSurface::of(&turn_tools),
    )
    .await;
    note_surfaced_lessons(service, &mut session, &turn_prompt);

    // Always set/update the system message so the agent has latest context
    session.set_system_message(&turn_prompt.stable);
    observe_system_prefix(&mut session, &turn_prompt.stable);

    // Update the temporal sidecar AFTER building the prompt. The temporal
    // context block reads this sidecar to compute "time since last
    // interaction"; writing `now` *before* the build would zero the gap
    // (and the builder suppresses gaps < 60s), so the block would never
    // appear and the agent would confabulate how long it's been.
    if let Err(e) = workforce_common::temporal::TemporalSidecar::write(
        &service.workspace,
        user,
        chrono::Utc::now(),
    ) {
        tracing::warn!(error = %e, "Failed to update temporal sidecar");
    }

    // Resolve model: explicit override > session > preference > agent > default.
    // The override is used by scheduled jobs that pin a specific model
    // (e.g. memory consolidation runs on `cheap`).
    let resolved_model = model_override
        .map(str::to_string)
        .unwrap_or_else(|| service.resolve_model(&session, None, Some(message)));

    // Reasoning depth for this turn. A spoken turn is judged on how fast
    // it starts talking, so `[llm] voice_thinking_budget_tokens` replaces
    // the client's configured budget for its duration; `None` leaves every
    // provider exactly as configured.
    let turn_thinking_budget = turn_thinking_budget(service, &session);
    let model_aware_config = service.compaction_config_for(&resolved_model).await;

    // Build runner config
    let config = RunnerConfig {
        agent_id: service.agent_id.clone(),
        system_prompt: turn_prompt.stable.clone(),
        model: Some(resolved_model.clone()),
        max_tokens: Some(
            service
                .active_max_tokens
                .read()
                .unwrap_or(service.effective_llm.max_tokens),
        ),
        max_rounds: resolve_max_rounds(
            max_rounds_override,
            *service.active_max_rounds.read(),
            origin_default_max_rounds(user, &service.effective_llm),
        ),
        team_id: None,
        role: None,
        compaction: model_aware_config.clone(),
        spill: service.current_spill_config(),
        routing: current_routing_config(service),
        tier_registry: service.tier_registry(),
        model_max_tokens: service.effective_llm.model_max_tokens.clone(),
        context_eviction_interval: None,
        context_eviction_keep_last: 3,
        trace_writer: service.trace_writer.clone(),
        trace_tags: trace_tags(service, channel, Some(message), None),
        permission_writer: service.permission_writer.read().clone(),
        permission_gate: service.permission_gate.read().clone(),
        arguments: service.config.tools.validation.mode,
        tool_loading: service.tool_loading(),
        programs: service.programs(),
        capability_gate: service.capability_gate.read().clone(),
        approval_router: service.approval_router.read().clone(),
        preexecute: workforce_agent::runner::CURRENT_PREEXECUTE
            .try_with(|p| p.clone())
            .ok(),
        journal: Some(turn_journal.clone()),
        collect,
        seeds: None,
        interactive: !is_automated_source(user),
        self_serve: workforce_agent::tools::approval::SelfServe::Answerable,
        compile_gate: service.compile_gate(),
    };

    let runner = runner_on(
        service,
        Arc::clone(&service.llm),
        config.clone(),
        &turn_tools,
    );
    let chain = turn_chain(service, &resolved_model).await;

    // Zone-based compaction: only block when urgently needed, defer otherwise
    let reserved = runner
        .reserved_context_for(&resolved_model)
        .with_prompt_tokens(estimate_tokens(&turn_prompt.turn_context));
    let needs_compaction = session.needs_compaction(&model_aware_config, reserved);
    let needs_urgent = session.needs_urgent_compaction(&model_aware_config, reserved);

    if needs_compaction {
        let memory_writer = crate::notes_tool::memory_writer(service);
        let compaction_engine = runner.compaction_engine_with_writer(memory_writer);

        if needs_urgent {
            // RED ZONE: must compact synchronously
            if let Err(e) = compaction_engine
                .compact_if_needed(&mut session, reserved)
                .await
            {
                tracing::warn!(error = %e, "Compaction failed, continuing with full session");
            }

            // If still massively over limit after emergency compaction, reset
            // the session with a carryover summary rather than letting the agent
            // respond with destroyed context.
            let post_tokens = session.estimated_total_with_reserved(reserved);
            if post_tokens > model_aware_config.max_context_tokens {
                tracing::warn!(
                    tokens = post_tokens,
                    max = model_aware_config.max_context_tokens,
                    "Session still over max after compaction — resetting with carryover"
                );

                let carryover = session
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.is_compaction_summary())
                    .map(|m| m.content.clone())
                    .unwrap_or_else(|| {
                        "Previous conversation context was too large and has been reset.".into()
                    });

                session.messages.retain(|m| {
                    m.role == workforce_common::session::MessageRole::System
                        && !m.is_compaction_summary()
                });
                let carryover_tokens = (carryover.len() / 4).max(1) as u32;
                session
                    .messages
                    .push(workforce_common::session::SessionMessage {
                        id: uuid::Uuid::new_v4().to_string(),
                        role: workforce_common::session::MessageRole::System,
                        content: format!("[Session reset due to context overflow]\n\n{carryover}"),
                        tool_data: None,
                        timestamp: chrono::Utc::now(),
                        tokens: carryover_tokens,
                        external_id: None,
                        attachments: None,
                        trust_level: None,
                        actor: None,
                        desk: None,
                    });
                session.total_tokens = session.estimated_total_tokens() as u64;

                tracing::info!(
                    tokens_after_reset = session.estimated_total_tokens(),
                    "Session reset complete"
                );
            }
        } else {
            // YELLOW ZONE: fast prune only, defer full compaction
            if let Err(e) = compaction_engine.compact_pruning_only(&mut session) {
                tracing::warn!(error = %e, "Pruning failed, continuing");
            }
            tracing::info!(
                estimated_tokens = session.estimated_total_tokens(),
                threshold = model_aware_config.trigger_threshold(),
                urgent = model_aware_config.urgent_threshold(),
                "Deferring full compaction to background (sync path)"
            );
        }
    }

    // Build conversation history from session (filters empty content,
    // merges consecutive same-role messages, skips tool/system messages)
    let shared_replay = shared_replay(service, &session, user);
    let prior_messages = build_prior_messages(&session.messages, shared_replay.as_ref());
    observe_replay_prefix(&mut session, &prior_messages);
    let session_tools = Arc::new(crate::tool_loading::session_tools(
        &session.messages,
        service.config.tools.loading.keep_turns,
    ));

    tracing::info!(
        messages = prior_messages.len(),
        session_total = session.messages.len(),
        "Loading conversation history (sync)"
    );

    // Set messaging context on scheduler so the scheduler tool can auto-capture it
    if let (Some(ref sched), Some(cid)) = (&service.scheduler, channel_id) {
        sched
            .set_messaging_context(channel, cid.to_string(), thread_id.map(|t| t.to_string()))
            .await;
    }

    // Run the agent with history (welcome-augmented message on first turn)
    // Wrap in task-local scope so tools can resolve the current session.
    let attachment_staging = SessionContext {
        id: Some(current_session_id.clone()),
        ..SessionContext::capture()
    };
    let task_content = build_task_content(
        turn_prompt.task_block(channel).as_deref(),
        injected_block.as_deref(),
        &effective_message,
        &attachments,
        &service.workspace,
        service.resolve_media_strategies(&chain.media_model(&resolved_model)),
        trust_level,
        &attachment_staging,
    )
    .await;
    let session_key_owned = channel.to_string();
    let session_id_owned = current_session_id.clone();
    let user_id_owned = principal.to_string();
    let env_scope = crate::environments::resolve_scope(service, Some(&session));
    let platform_user_id = service.resolve_platform_user_id(principal);
    let started_at = chrono::Utc::now();
    let turn_served = ServedLog::default();
    let continuity = crate::agent_turn::Continuity::of(&session);
    let outcome = CURRENT_SESSION_KEY
        .scope(
            session_key_owned,
            CURRENT_SESSION_ID.scope(
                session_id_owned,
                CURRENT_USER_ID.scope(
                    user_id_owned,
                    ACTIVE_ENVIRONMENT.scope(
                        env_scope.environment.unwrap_or_default(),
                        ACTIVE_SKILL_INSTANCES.scope(
                            env_scope.skill_instances,
                            ACTIVE_MCP_ENVS.scope(
                                env_scope.mcp_envs,
                                SESSION_TOOLS.scope(
                                    session_tools,
                                    CURRENT_PLATFORM_USER_ID.scope(
                                        platform_user_id,
                                        with_thinking_budget_override(
                                            turn_thinking_budget,
                                            PRECURATED_TOOLSET.scope(
                                                turn_tools.exclusive,
                                                turn_served.clone().scope(Box::pin(chain.run(
                                                    service,
                                                    crate::agent_turn::TurnRun {
                                                        config,
                                                        tools: turn_tools.clone(),
                                                        task: task_content,
                                                        prior: prior_messages,
                                                        session: &session,
                                                        replay: shared_replay.as_ref(),
                                                        progress: None,
                                                        cancel: None,
                                                        inject: None,
                                                        principal,
                                                        oversight: crate::agent_turn::Oversight::Attended(
                                                            crate::mcp_gate::RouteResume::InSession,
                                                        ),
                                                        continuity,
                                                    },
                                                ))),
                                            ),
                                        ),
                                    ),
                                ),
                            ),
                        ),
                    ),
                ),
            ),
        )
        .await?;

    // Clear messaging context after agent run
    if let Some(ref sched) = &service.scheduler {
        sched.clear_messaging_context(channel).await;
    }

    turn_journal
        .rows()
        .into_iter()
        .for_each(|row| session.push_message(row));

    let (mut result, route) = match outcome {
        crate::agent_turn::ChainOutcome::Served { result, route } => (*result, route),
        crate::agent_turn::ChainOutcome::Notice { text } => {
            save_notice_turn(service, &mut session, archive_base, user, &text, None)?;
            turn_journal.clear();
            return Ok(TurnOutcome::Answered(text));
        }
    };

    // Routed approvals raised by this (non-progress) run have no progress
    // consumer to DM the approvers — deliver them here, before the reply
    // pipeline. Zero-cost when the turn routed nothing.
    crate::approvals::slack::deliver_routed_for_invocations(service, &result.tool_invocations)
        .await;

    if post_answer_reviews_eligible(&session) {
        let answer = result.agent_response().into_owned();
        let (data_grounding, grounding, commitment) = tokio::join!(
            data_claim_grounding_block(
                service,
                &effective_message,
                &answer,
                &current_session_id,
                principal,
                &result.tool_invocations,
                None,
            ),
            claim_grounding_block(
                service,
                &question_only,
                &answer,
                &injected_source,
                &result.tool_invocations,
                None
            ),
            commitment_caveat_block(
                service,
                &answer,
                &result.tool_invocations,
                Origin::of_turn(user, channel, session.is_voice_turn()),
            ),
        );
        let labelled = [
            (
                "failed tools",
                failed_tools_notice(service, &result.tool_invocations),
            ),
            ("data grounding", data_grounding),
            ("source grounding", grounding.footer()),
            ("commitments", commitment),
        ]
        .into_iter()
        .filter_map(|(label, block)| block.map(|b| (label, b)))
        .collect();
        // Joined for parity with the streaming path: one annotation region,
        // budget + drop-notice semantics preserved by joining the OUTPUT.
        let segments = footer_segments(labelled);
        if !segments.is_empty() {
            result.response.push_str("\n\n");
            result.response.push_str(&segments.join("\n\n"));
        }
    } else {
        tracing::info!(target: "claim_grounding", agent = %service.agent_id, "skipped: spoken turn — review footers are never spoken");
    }

    let telemetry_provider = settle_served(&mut session, &result, route, reserved, None);

    // Record cost in the cost tracker — use the actual model from the run
    // (may differ from resolved_model if failover occurred)
    let cost_model = result.model.clone();
    let tool_names: Vec<&str> = result
        .tool_invocations
        .iter()
        .map(|t| t.tool_name.as_str())
        .collect();
    let task_type = infer_task_type(&effective_message, &tool_names);
    let identity_id = crate::user_config::current_billing_identity_id();
    let description = truncate_with_ellipsis(&effective_message, 200);
    service.cost_tracker.record(CostEntry {
        task_id,
        task_type: Some(&task_type),
        identity_id: identity_id.as_deref(),
        description: Some(&description),
        served: turn_served.served(),
        ..CostEntry::new(
            service.agent_id.as_str(),
            &cost_model,
            TokenCounts::from(&result.total_usage),
            Origin::of_turn(user, channel, session.is_voice_turn()),
        )
    });
    record_telemetry(
        service,
        &result,
        telemetry_provider,
        started_at,
        &task_type,
        None,
        Some(&description),
        task_id,
    );

    // Attribute this turn's messages to the asker so a shared session's
    // later replay can scope each speaker and the leak-safe tool filter.
    if session.is_shared_conversation() {
        session.attribute_messages_from(archive_base, user);
    }

    record_round_cap_continuation(&mut session, &result);
    service.sessions.save(&session)?;
    turn_journal.clear();
    persist_turn_owned_metadata(&service.sessions, &session);

    for msg in session.messages.get(archive_base..).unwrap_or_default() {
        if let Err(e) = service.sessions.append_to_archive(&session.id, msg) {
            warn!(error = %e, "Failed to append to session archive");
        }
    }

    // Fire-and-forget post-turn extraction (passive memory + DM-only
    // relationship capture). Shared with `process_message_with_progress` via
    // `spawn_turn_extraction` so the two interactive paths stay in lockstep.
    spawn_turn_extraction(
        service,
        &session,
        archive_base,
        &effective_message,
        &result.response,
        principal,
    );
    spawn_know_how(
        service,
        &session,
        principal,
        message,
        &result.tool_invocations,
    )
    .await;

    // Fire-and-forget lesson extraction + reinforcement. Both are
    // bounded — extraction LLM-calls only on detector-positive turns,
    // reinforcement is a no-op when nothing was injected this turn.
    crate::lessons::run_post_turn(
        crate::lessons::LessonPipelineConfig {
            // A shared session belongs to no single user; distilling its turns
            // into the agent's recallable lessons would widen the room's
            // content. Reinforcement self-gates (nothing is injected in a
            // shared room, so it no-ops).
            extract_enabled: service.effective_memory().lesson_extract
                && !session.is_shared_conversation(),
            llm: &service.llm,
            store: service.memory_store.as_ref(),
            agent_id: &service.agent_id,
            extract_model: lesson_extract_model(service),
            extract_prompt: service.effective_memory().lesson_extract_prompt.clone(),
            cost_tracker: &service.cost_tracker,
            sessions: &service.sessions,
        },
        &mut session,
        archive_base,
        &effective_message,
        &result.response,
    );

    // Fire-and-forget background compaction if deferred
    if needs_compaction && !needs_urgent {
        let bg_sessions = Arc::clone(&service.sessions);
        let bg_session_id = session.id.clone();
        let bg_llm = Arc::clone(&service.llm);
        let bg_model = Some(cost_model.clone());
        let bg_config = model_aware_config.clone();
        let bg_workspace = service.workspace.clone();
        let bg_memory_store = service.memory_store.clone();
        let bg_agent_id = service.agent_id.clone();
        let bg_cost_tracker = Arc::clone(&service.cost_tracker);
        let bg_reserved = reserved;
        spawn_in_session_context(async move {
            let mut fresh = match bg_sessions.get(&bg_session_id) {
                Ok(Some(s)) => s,
                _ => return,
            };
            let writer: Arc<dyn workforce_agent::compaction::MemoryWriter> = match bg_memory_store {
                Some(store) => Arc::new(crate::notes_tool::DailyNotesMemoryWriter::with_store(
                    bg_workspace,
                    store,
                    bg_agent_id.clone(),
                )),
                None => Arc::new(crate::notes_tool::DailyNotesMemoryWriter::new(bg_workspace)),
            };
            let engine = workforce_agent::compaction::CompactionEngine::with_memory_writer(
                bg_config, bg_llm, bg_model, writer,
            )
            .with_cost_tracker(bg_cost_tracker, bg_agent_id.to_string());
            match engine.compact_if_needed(&mut fresh, bg_reserved).await {
                Ok(true) => {
                    if let Err(e) = bg_sessions.save(&fresh) {
                        tracing::warn!(error = %e, "Background compaction (sync): save failed");
                    } else {
                        tracing::info!(
                            session_id = %bg_session_id,
                            tokens = fresh.estimated_total_tokens(),
                            "Background compaction complete (sync path)"
                        );
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "Background compaction (sync) failed");
                }
            }
        });
    }

    // Proactive thread rollover: if the session built rollover pressure
    // this turn — token watermark, or message-count ceiling for voice —
    // (and didn't compact; the two are mutually exclusive), seal it and
    // mint a fresh successor so the next turn starts small. The next turn
    // resolves the successor via `get_or_create`. The cheap pressure
    // pre-check avoids building an engine every turn.
    if !needs_compaction
        && workforce_agent::rollover::rollover_pressure(&session, &model_aware_config, reserved)
    {
        workforce_agent::rollover::maybe_roll_over(
            Arc::clone(&service.sessions),
            runner.compaction_engine(),
            &mut session,
            &model_aware_config,
            reserved,
        );
    }

    let stopped = is_automated_source(user)
        .then(|| {
            crate::cap_gate::stopped(
                service.identity_store.read().as_ref(),
                service.capability_gate.read().as_deref(),
                principal,
                &result.tool_invocations,
            )
        })
        .flatten();
    Ok(match stopped {
        Some(stop) => TurnOutcome::Stopped(stop),
        None => TurnOutcome::Answered(result.response),
    })
}

/// Run an isolated sub-agent session.
///
/// Creates a fresh session (kind: SubAgent), builds a runner with an
/// optional model override, executes the task, and returns the response.
/// Used by the `subagent` tool for background task execution.
pub(crate) async fn run_isolated(
    service: &Arc<AgentService>,
    channel: &str,
    task: &str,
    model_override: Option<&str>,
    system_prompt_override: Option<&str>,
    run: &IsolatedRun,
    profile: crate::job_runner::AgentProfile,
    curation: Option<(
        crate::hub_core::curation::CurationCtx,
        workforce_common::learning_plan::CurationMode,
    )>,
    task_label: Option<&str>,
) -> Result<crate::hub_core::jobs::AgentRunResult> {
    use crate::job_runner::AgentProfile;
    use crate::user_config::as_principal;
    use workforce_common::session::SessionKind;

    let principal = &run.principal;
    // Mandatory budget admission — mirrors `process_message` so isolated
    // work (subagents, curation) cannot spend past a denied scope.
    let budget = admit_turn_or_deny(service, principal.run_as.as_deref(), None);
    if let Some(denied) = as_principal(principal, budget).await {
        eyre::bail!("{denied}");
    }

    // Create isolated session
    let mut session =
        service
            .sessions
            .get_or_create(channel, "subagent", service.agent_id.as_str())?;
    session.kind = SessionKind::SubAgent;

    // Add user message (the task)
    session.add_user_message(task, None);

    // Build system prompt
    let system_prompt = if let Some(override_prompt) = system_prompt_override {
        override_prompt.to_string()
    } else if profile == AgentProfile::Worker {
        if worker_read_sources_enabled(service) {
            format!("{WORKER_SYSTEM_PROMPT}{WORKER_READ_SOURCES_HINT}")
        } else {
            WORKER_SYSTEM_PROMPT.to_string()
        }
    } else {
        build_enhanced_prompt(service, None, None, false).await
    };
    session.set_system_message(&system_prompt);

    // Resolve the model: an explicit override wins; a worker defaults to the
    // cheap tier (falling back to the normal resolver when the active tier
    // has no cheap mapping); otherwise the agent's normal resolution applies.
    let model = match model_override {
        Some(m) => m.to_string(),
        None if profile == AgentProfile::Worker => {
            let tier_key = service.tier_key.read().clone();
            let resolved = service
                .tier_registry()
                .resolve_model(WORKER_MODEL_TIER, &tier_key);
            if resolved == WORKER_MODEL_TIER {
                service.resolve_model(&session, None, Some(task))
            } else {
                resolved
            }
        }
        None => service.resolve_model(&session, None, Some(task)),
    };

    let config = RunnerConfig {
        agent_id: service.agent_id.clone(),
        system_prompt,
        model: Some(model.clone()),
        max_tokens: Some(
            service
                .active_max_tokens
                .read()
                .unwrap_or(service.effective_llm.max_tokens),
        ),
        max_rounds: {
            let configured = service
                .active_max_rounds
                .read()
                .unwrap_or(service.effective_llm.max_rounds);
            match profile {
                AgentProfile::Worker => configured.min(WORKER_MAX_ROUNDS),
                // Uncapped Curator turns hit the deployment-wide 50-round
                // budget in production at ~$5/run; `[self_learning]
                // max_rounds` (default 24) bounds them.
                AgentProfile::Curator => {
                    configured.min(service.effective_self_learning().max_rounds)
                }
                AgentProfile::Full => configured,
            }
        },
        team_id: None,
        role: None,
        compaction: service.current_compaction_config(),
        spill: service.current_spill_config(),
        routing: current_routing_config(service),
        tier_registry: service.tier_registry(),
        model_max_tokens: service.effective_llm.model_max_tokens.clone(),
        // Curation turns accumulate tens of rounds of tool output with
        // the whole prompt re-billed every round; periodic eviction keeps
        // that quadratic term bounded. Other isolated runs keep the
        // legacy no-eviction behaviour.
        context_eviction_interval: matches!(profile, AgentProfile::Curator)
            .then_some(CURATOR_EVICTION_INTERVAL),
        context_eviction_keep_last: 3,
        trace_writer: service.trace_writer.clone(),
        trace_tags: trace_tags(
            service,
            channel,
            None,
            Some(task_label.unwrap_or("subagent")),
        ),
        permission_writer: service.permission_writer.read().clone(),
        permission_gate: service.permission_gate.read().clone(),
        arguments: service.config.tools.validation.mode,
        tool_loading: service.tool_loading(),
        programs: service.programs(),
        capability_gate: service.capability_gate.read().clone(),
        // Isolated sub-agent runs never route approvals: no delivery
        // path follows this runner (only `process_message` delivers
        // routed DMs post-run), so a routed row here would sit
        // undelivered until sweeper expiry. Unrouted, a gated call gets
        // the instant "Approval required" error, which the sub-agent
        // reports back to its caller.
        approval_router: None,
        preexecute: workforce_agent::runner::CURRENT_PREEXECUTE
            .try_with(|p| p.clone())
            .ok(),
        journal: None,
        collect: Vec::new(),
        seeds: None,
        interactive: false,
        self_serve: workforce_agent::tools::approval::SelfServe::Answerable,
        // A worker's and a curator's tools only read.
        compile_gate: matches!(profile, AgentProfile::Full)
            .then(|| service.compile_gate())
            .flatten(),
    };

    // A worker sees only the read-only research subset; a full agent sees
    // the complete registry. `install_tool_access` wraps the runner without
    // rebinding tools, so the worker restriction holds.
    let tools = match profile {
        AgentProfile::Worker => {
            // A worker's registry is a *filtered snapshot* of `service.tools`,
            // and a worker's allowlist has no `system.enable_skill`, so it
            // cannot load a service adapter itself. Native adapters register
            // lazily (only when `enable_skill` runs), so unless one was
            // enabled earlier in this process the snapshot is empty of it
            // and the worker is stranded — e.g. asked to use Linear it finds
            // no `linear.*` tool and confabulates that the integration does
            // not exist (driftwerk-prod, 2026-06-10). Pre-load configured
            // read-only adapters into the shared registry first, scoping the
            // user id so per-user creds resolve (mirrors the run scope set
            // below). Covers both the `subagent profile=worker` path and the
            // `investigate` fan-out — both run through here.
            ensure_worker_adapters_loaded_as(service, principal);
            // The snapshot is the worker's hard read-only boundary. When
            // `[worker_read_sources]` is enabled, give it the `read_source.*`
            // tools so the worker can pull additional read-only surfaces
            // (AWS/Slack/GitHub/Notion/Google) into THIS snapshot on demand,
            // on top of the base set — read-only is enforced at the enable
            // boundary (see `crate::worker_read`). Off by default.
            let snapshot = Arc::new(worker_tool_registry(service));
            if worker_read_sources_enabled(service) {
                register_worker_read_sources(service, &snapshot);
            }
            snapshot
        }
        // The curation read-only snapshot (see `curation_tool_registry`),
        // plus opt-in `read_source.*` like a worker.
        AgentProfile::Curator => {
            ensure_worker_adapters_loaded_as(service, principal);
            let snapshot = Arc::new(curation_tool_registry(service, curation));
            if worker_read_sources_enabled(service) {
                register_worker_read_sources(service, &snapshot);
            }
            snapshot
        }
        AgentProfile::Full => Arc::clone(&service.tools),
    };
    // A worker/curator registry is already curated to a deliberate subset,
    // so its tools are offered whole rather than catalogued (see
    // `PRECURATED_TOOLSET`).
    let precurated = matches!(profile, AgentProfile::Worker | AgentProfile::Curator);
    let tools = TurnTools {
        registry: tools,
        exclusive: precurated,
    };
    let chain = match &run.route {
        IsolatedRoute::Agent => turn_chain(service, &model).await,
        IsolatedRoute::Pinned(spec) => pinned_chain(service, spec),
    };
    let tool_principal = principal
        .run_as
        .clone()
        .unwrap_or_else(|| format!("agent:{}", service.agent_id.as_str()));

    // Run agent with empty history (fresh session). Wrap in task-local
    // scopes so tools see the current agent, session AND user identity.
    // run_isolated is called from `tokio::spawn`'d futures (e.g.,
    // subagent dispatch) where the parent's task-locals are gone, so
    // the caller passes its principal explicitly. The agent id is scoped too,
    // otherwise agent-context tools (e.g. `data.query` resolving staged
    // artifacts) fail with "no agent context in scope". Per-user vault
    // paths in built-in adapters and WASM skills hard-fail when user_id
    // is missing, so callers must either pass a real uid or wrap their
    // call in `with_system_identity`.
    let agent_id_owned = service.agent_id.as_str().to_string();
    let session_key_owned = channel.to_string();
    let session_id_owned = session.id.clone();
    let started_at = chrono::Utc::now();
    // Reset the environment to the global DEFAULT for the isolated run so a
    // sub-agent / research worker never inherits the parent's `cmd: env use`
    // (or `cmd: use`) override — a human's staging activation must not
    // silently follow a spawned investigation. run_isolated is reached via
    // `tokio::spawn` so the parent's task-locals are already gone; resetting
    // here makes non-inheritance an explicit guarantee of run_isolated
    // itself, independent of the spawn boundary. With `[environments]` off
    // this short-circuits to empty maps — only the MCP workspace default is
    // active, exactly as before the feature existed.
    let env_scope = crate::environments::resolve_scope(service, None);
    let run_served = ServedLog::default();
    let inner = CURRENT_AGENT_ID.scope(
        agent_id_owned,
        CURRENT_SESSION_KEY.scope(
            session_key_owned,
            CURRENT_SESSION_ID.scope(
                session_id_owned,
                ACTIVE_ENVIRONMENT.scope(
                    env_scope.environment.unwrap_or_default(),
                    ACTIVE_SKILL_INSTANCES.scope(
                        env_scope.skill_instances,
                        ACTIVE_MCP_ENVS.scope(
                            env_scope.mcp_envs,
                            run_served.clone().scope(Box::pin(chain.run(
                                service,
                                crate::agent_turn::TurnRun {
                                    config,
                                    tools,
                                    task: vec![ContentBlock::Text {
                                        text: task.to_string(),
                                    }],
                                    prior: Vec::new(),
                                    session: &session,
                                    replay: None,
                                    progress: None,
                                    cancel: None,
                                    inject: None,
                                    principal: &tool_principal,
                                    oversight: crate::agent_turn::Oversight::Unattended,
                                    continuity: crate::agent_turn::Continuity::OneShot,
                                },
                            ))),
                        ),
                    ),
                ),
            ),
        ),
    );
    let (result, route) = match PRECURATED_TOOLSET
        .scope(precurated, as_principal(principal, inner))
        .await?
    {
        crate::agent_turn::ChainOutcome::Served { result, route } => (*result, route),
        crate::agent_turn::ChainOutcome::Notice { text } => eyre::bail!("{text}"),
    };

    // Record tool invocations
    for invocation in &result.tool_invocations {
        session.add_tool_message(
            &invocation.tool_name,
            &invocation.input,
            &invocation.output,
            invocation.success,
            invocation.duration_ms,
            invocation.error.as_deref(),
        );
    }

    // Save response (secrets masked at rest)
    add_redacted_assistant_message(&mut session, &result.response, None);
    service.sessions.save(&session)?;

    // Record cost for sub-agent run — use actual model after failover.
    // `task_label` keys per-source attribution (e.g. a self-learning plan
    // records as `self_learning:{plan_id}` so its spend is forecastable).
    let description = truncate_with_ellipsis(task, 200);
    let task_type = task_label.unwrap_or("subagent");
    let origin = match (profile, task_label) {
        (AgentProfile::Curator, _) => Origin::SelfLearning,
        (_, Some(label)) if label.starts_with(crate::self_learning::TASK_LABEL_PREFIX) => {
            Origin::SelfLearning
        }
        _ => Origin::Subagent,
    };
    service.cost_tracker.record(CostEntry {
        task_type: Some(task_type),
        identity_id: principal.billing(),
        description: Some(&description),
        served: run_served.served(),
        ..CostEntry::new(
            service.agent_id.as_str(),
            &result.model,
            TokenCounts::from(&result.total_usage),
            origin,
        )
    });
    record_telemetry(
        service,
        &result,
        route.label(),
        started_at,
        task_type,
        None,
        Some(&description),
        None,
    );

    // Collate any findings the worker recorded into one artifact. Staged
    // here — outside the run's session scope, where the detached worker has
    // no ambient session — so the artifact is addressable by its handle
    // across sessions, which is how the spawning agent reaches it.
    let collated = crate::findings::collate_findings(
        workforce_agent::artifacts::global_store().as_deref(),
        service.agent_id.as_str(),
        &result.tool_invocations,
    );
    Ok(crate::hub_core::jobs::AgentRunResult {
        response: result.response,
        findings: collated.staged,
        claims: collated.findings,
        package: None,
    })
}

/// [`Self::ensure_adapters_loaded`] for a worker's read-only adapters, as
/// `principal`, so per-user credentials resolve for the identity the run
/// acts as.
///
/// Native adapters (linear/jira/datadog/metabase/postmark/sendgrid) register lazily —
/// only when `system.enable_skill` runs (DW-277 deferred loading). A worker's
/// allowlist has no `system.enable_skill`, so it cannot load one itself, and
/// [`worker_tool_patterns`] (which unions in each adapter's read tools) only
/// surfaces a tool that is *already* in `self.tools`. Only adapters that
/// contribute worker-usable read tools are loaded (so write-only/non-research
/// adapters like github/google/email and heavyweight ones stay deferred).
pub(crate) fn ensure_worker_adapters_loaded_as(service: &AgentService, principal: &TurnPrincipal) {
    workforce_agent::runner::CURRENT_USER_ID
        .sync_scope(principal.run_as.clone().unwrap_or_default(), || {
            crate::skill_loader::ensure_adapters_loaded(service, adapter_contributes_worker_reads)
        })
}

/// Build the read-only tool registry for a worker sub-agent from this
/// service's full registry (see [`WORKER_TOOL_PATTERNS`]), plus the
/// worker-only `report_findings` tool.
pub(crate) fn worker_tool_registry(
    service: &AgentService,
) -> workforce_agent::tools::registry::ToolRegistry {
    let registry = crate::mcp::build_mcp_registry(&service.tools, &worker_tool_patterns());
    registry.register(Arc::new(crate::findings::ReportFindingsTool));
    registry
}

/// Build the read-only tool registry for a self-learning curation turn.
///
/// Starts from the worker read-only research set, positively adds the
/// read-only `database.*` tools (which self-enforce read-only — a write
/// needs `DatabaseWrite` + `allow_writes`, which the curation identity
/// lacks), and registers the single `self_learning.record` write tool —
/// which writes only to the knowledge store, never to a source. NO
/// write/destructive/execute/`vault.*`/messaging tool is ever added: the
/// returned registry IS the read-only boundary (design §6.1).
/// The read-only tool surface for a curation turn. Built from
/// [`curation_tool_patterns`] (NOT the broader worker set — it must exclude
/// `memory`/`notes`/`context.*`, see [`CURATION_TOOL_PATTERNS`]), plus the
/// read-only `database.*` tools and the single `self_learning.record` write.
/// This registry IS the read-only boundary: a tool not in it is uncallable.
pub(crate) fn curation_tool_registry(
    service: &AgentService,
    curation: Option<(
        crate::hub_core::curation::CurationCtx,
        workforce_common::learning_plan::CurationMode,
    )>,
) -> workforce_agent::tools::registry::ToolRegistry {
    use workforce_common::learning_plan::CurationMode;
    let registry = crate::mcp::build_mcp_registry(&service.tools, &curation_tool_patterns());
    for name in CURATION_DATABASE_READ_TOOLS {
        if let Some(tool) = service.tools.get(name) {
            registry.register(tool);
        }
    }
    // The read-only research tools above are identical for every mode; only
    // the single write tool differs. `Items` registers the atomic-fact tool
    // exactly as before; both tree modes (`DeepExpertise` from real source,
    // `ExperientialExpertise` from session transcripts) register the same
    // tree-node tool against the agent's deep-expertise view of the store —
    // they differ only in the producer's prompt and grounding, not the write
    // surface.
    if let (Some(memory), Some((ctx, mode))) = (service.memory_store.clone(), curation) {
        match mode {
            CurationMode::Items => {
                if let Some(store) = service.self_learning_store() {
                    registry.register(Arc::new(crate::self_learning::SelfLearningRecordTool::new(
                        store,
                        memory,
                        service.effective_self_learning(),
                        ctx,
                    )));
                }
            }
            CurationMode::DeepExpertise | CurationMode::ExperientialExpertise => {
                if let Some(store) = service.deep_expertise_store() {
                    registry.register(Arc::new(crate::self_learning::DeepNodeRecordTool::new(
                        store,
                        memory,
                        service.effective_self_learning(),
                        ctx,
                    )));
                }
            }
        }
    }
    registry
}

/// Register the `read_source.*` tools into a worker's snapshot. They hold an
/// `Arc` to the snapshot itself (the registration target) and to the global
/// registry (the copy source), so a worker can materialise an additional
/// read-only source's tools into its own snapshot on demand. Read-only is
/// enforced at the enable boundary in [`crate::worker_read`].
pub(crate) fn register_worker_read_sources(
    service: &AgentService,
    snapshot: &Arc<workforce_agent::tools::registry::ToolRegistry>,
) {
    snapshot.register(Arc::new(crate::worker_read::ReadSourceListTool::new(
        Arc::clone(snapshot),
        Arc::clone(&service.tools),
        service.vault.clone(),
        service.global_vault.clone(),
    )));
    snapshot.register(Arc::new(crate::worker_read::ReadSourceEnableTool::new(
        Arc::clone(snapshot),
        Arc::clone(&service.tools),
        service.vault.clone(),
        service.global_vault.clone(),
        Arc::clone(&service.config),
    )));
}

/// Process a message with progress events streamed to the caller.
///
/// Returns a channel receiver that emits `ProgressEvent`s during processing,
/// a join handle for the async task, and the session ID being used.
/// If `session_id` is provided, it attempts to resume that session;
/// otherwise creates/reuses the default session for the channel+user.
///
/// Slash commands are intercepted and return immediately with a
/// `ProgressEvent::Response` containing the command output.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_message_with_progress(
    service: &Arc<AgentService>,
    channel: &str,
    user: &str,
    run_as: Option<&str>,
    message: &str,
    external_id: Option<&str>,
    session_id: Option<&str>,
    attachments: Option<Vec<SessionAttachment>>,
    channel_system_prompt: Option<&str>,
    channel_id: Option<&str>,
    thread_id: Option<&str>,
    inbox: Option<TurnInbox>,
    display_content: Option<&str>,
    ticket_id: Option<&str>,
    step_max_rounds: Option<u32>,
    step_model: Option<&str>,
    step_context_eviction_interval: Option<u32>,
    step_context_eviction_keep_last: Option<u32>,
    tool_whitelist: Option<&[String]>,
    persona_override: Option<&str>,
    workspace_id: Option<&str>,
    trust_level: Option<workforce_common::trust::TrustLevel>,
    cancel_token: Option<tokio_util::sync::CancellationToken>,
    voice_channel: Option<&str>,
    desk: Option<&workforce_common::channels::desk::DeskTurnContext>,
) -> Result<MessageProgress> {
    // One token per run, end-to-end: a caller that claimed the runner slot
    // before setup passes its token so stop/hard-stop reaches the runner
    // even while setup is still in flight.
    let cancel_token = cancel_token.unwrap_or_default();
    // The identity the turn runs *as* (credential, memory, and capability
    // scoping) — mirrors `process_message`. `user` keeps recording the
    // triggering source (session keying, automated-source detection).
    let principal = run_as_principal(run_as, user);
    // Defence-in-depth tokenization for callers that didn't
    // tokenize at the surface (gateway, chat_worker, voice,
    // workflow_runtime self-dispatch). Slash commands run on raw
    // text and may legitimately consume cleartext.
    let tokenized;
    let message: &str = if crate::commands::parse_command(message).is_some() {
        message
    } else {
        tokenized = workforce_common::redact::tokenize_secrets(
            message,
            channel,
            workforce_common::secret_marker::SecretSource::Unknown,
        )
        .0;
        &tokenized
    };

    // Check for slash commands first
    if let Some(cmd) = crate::commands::parse_command(message) {
        let service = Arc::clone(service);
        let channel_owned = channel.to_string();
        let user_owned = user.to_string();

        let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(64);

        // Get session ID for the response tuple
        let sid = session_id
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{}:{}", channel, user));

        let handle = tokio::spawn(async move {
            let result =
                crate::commands::execute_command(&cmd, &service, &channel_owned, &user_owned).await;
            let _ = progress_tx
                .send(ProgressEvent::Response {
                    text: result.text.clone(),
                })
                .await;
            let _ = progress_tx
                .send(ProgressEvent::Done {
                    rounds: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    tool_count: 0,
                })
                .await;
            Ok(reply_only(result.text))
        });

        // Slash commands run on raw text and never assemble code context, so
        // they carry no citations.
        return Ok(MessageProgress {
            progress_rx,
            handle,
            session_id: sid,
            turn_token: String::new(),
            turn_claim_id: None,
            cancel_token,
            code_citations: Vec::new(),
            served: ServedLog::default(),
        });
    }

    // Mandatory budget admission — mirrors `process_message`; a denied
    // turn streams the refusal as its whole response.
    if let Some(denied) = admit_turn_or_deny(service, Some(principal), channel_id).await {
        let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(64);
        let sid = session_id
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{}:{}", channel, user));
        let handle = tokio::spawn(async move {
            let _ = progress_tx
                .send(ProgressEvent::Response {
                    text: denied.clone(),
                })
                .await;
            let _ = progress_tx
                .send(ProgressEvent::Done {
                    rounds: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    tool_count: 0,
                })
                .await;
            Ok(reply_only(denied))
        });
        return Ok(MessageProgress {
            progress_rx,
            handle,
            session_id: sid,
            turn_token: String::new(),
            turn_claim_id: None,
            cancel_token,
            code_citations: Vec::new(),
            served: ServedLog::default(),
        });
    }

    // Everything after admission runs as the principal, as in
    // `process_message`: urgent compaction, attachments and context
    // assembly before the spawn see who the turn is for.
    SessionContext {
        user_id: Some(principal.to_string()),
        agent_id: Some(service.agent_id.as_str().to_string()),
        ..SessionContext::capture()
    }
    .scope(Box::pin(stream_admitted(
        service,
        StreamedTurn {
            channel,
            user,
            principal,
            message,
            external_id,
            session_id,
            attachments,
            channel_system_prompt,
            channel_id,
            thread_id,
            inbox,
            display_content,
            ticket_id,
            step_max_rounds,
            step_model,
            step_context_eviction_interval,
            step_context_eviction_keep_last,
            tool_whitelist,
            persona_override,
            workspace_id,
            trust_level,
            cancel_token,
            voice_channel,
            desk,
        },
    )))
    .await
}

/// The body of [`Self::process_message_with_progress`] once the turn is
/// admitted.
pub(crate) async fn stream_admitted(
    service: &Arc<AgentService>,
    turn: StreamedTurn<'_>,
) -> Result<MessageProgress> {
    let StreamedTurn {
        channel,
        user,
        principal,
        message,
        external_id,
        session_id,
        attachments,
        channel_system_prompt,
        channel_id,
        thread_id,
        inbox,
        display_content,
        ticket_id,
        step_max_rounds,
        step_model,
        step_context_eviction_interval,
        step_context_eviction_keep_last,
        tool_whitelist,
        persona_override,
        workspace_id,
        trust_level,
        cancel_token,
        voice_channel,
        desk,
    } = turn;
    let (inject_rx, self_serve) = inbox.map_or((None, Default::default()), |inbox| {
        (Some(inbox.rx), inbox.self_serve)
    });
    let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(64);

    // Setup cancellation checkpoints: a claimed-slot caller (Slack) may
    // hard-stop this turn while it is still assembling context — bail at
    // phase boundaries instead of finishing a run nobody wants.
    if cancel_token.is_cancelled() {
        eyre::bail!("turn cancelled during setup");
    }

    // Get session: by explicit ID, or get/create for channel+owner. A shared
    // room keys by thread (not by user) so every participant continues one
    // conversation.
    let owner = session_owner_for_turn(channel, channel_id, thread_id, external_id, user);
    let session = match session_id
        .map(|sid| service.sessions.get(sid))
        .transpose()?
    {
        // A sealed session has rolled over; route to the thread's active
        // successor so a client still holding the old id lands on the live
        // one instead of resuming the dead segment.
        Some(Some(s)) if s.is_sealed() => {
            service
                .sessions
                .get_or_create(channel, &owner, service.agent_id.as_str())?
        }
        Some(Some(s)) => s,
        Some(None) => {
            let mut s =
                workforce_common::session::Session::new(channel, &owner, service.agent_id.as_str());
            s.label = Some(format!("{}:{}", channel, owner));
            s
        }
        None => service
            .sessions
            .get_or_create(channel, &owner, service.agent_id.as_str())?,
    };
    let mut session = service.roll_stale_for_resume(channel, session)?;
    let current_session_id = session.id.clone();

    // One live turn per session across the fleet: the claim is held (and
    // renewed) for the whole turn by the spawned task below, and its
    // absence means another hub instance is already mid-turn here — e.g.
    // the boot resume sweep racing a turn still draining on the old
    // instance during a rolling deploy.
    let turn_claim = crate::session_turn_claim::SessionTurnClaim::acquire_or_held(
        &service.sessions,
        &current_session_id,
    )?;

    // Classify the session's context once (DM vs shared channel/thread) so
    // relationship capture (DM-only) and per-type reset policies have a
    // persisted signal. Idempotent: only the first turn sets it. Mirrors
    // `process_message`; this path (Slack/web/workflows) is where real
    // interactive traffic flows, so it must classify too.
    if session.context_type.is_none() {
        session.context_type = Some(classify_context_type(channel_id, thread_id));
    }

    // Build augmented message with channel context and welcome directive.
    let is_new_session = session.messages.is_empty();
    let mut effective_message = message.to_string();

    // Inject messaging context so the agent knows where it is for tool calls:
    // the Slack channel and thread, or the Desk conversation and speaker.
    let person = desk.and_then(|_| {
        crate::desk_surface::person_name(service.identity_store.read().as_ref(), principal)
    });
    if let Some(block) = surface::surface_context(
        &surface::TurnSurface::of(channel_id, thread_id, desk),
        channel,
        &current_session_id,
        user,
        person.as_deref(),
    ) {
        effective_message = format!("{block}\n\n{effective_message}");
    }
    if let Some(desk) = desk {
        session.set_desk_origin(&desk.target);
    }

    // Prepend channel-specific system prompt if configured
    if let Some(prompt) = channel_system_prompt {
        effective_message = format!("[CHANNEL CONTEXT]\n{}\n\n{}", prompt, effective_message);
    }

    // On first message in a new user-conversation session, prepend the
    // welcome directive and inject prior Slack-thread history so the
    // agent picks up after a `/new`. Internal channels (workflow steps,
    // schedulers, subagents, consolidation, knowledge-maintenance) share
    // the originating Slack thread for status output but are NOT
    // continuations of the user conversation — injecting the thread there
    // leaks unrelated topics into task-focused step prompts (the
    // workflow-step-answers-prior-question incident).
    if should_inject_session_intro(channel, is_new_session, channel_id, thread_id) {
        if let Some(welcome) = service.welcome().await {
            effective_message = format!(
                "[WELCOME DIRECTIVE — first message in this channel, incorporate a brief greeting]\n{}\n\n[User's message]\n{}",
                welcome, effective_message
            );
        }

        if let (Some(cid), Some(tid)) = (channel_id, thread_id) {
            let surfaces = service.surface_registry.read().await;
            let hint = workforce_common::channels::ChannelHint::default();
            let resolved = surfaces.resolve("slack", cid, hint).await;
            drop(surfaces);
            if let Ok(surface) = resolved {
                match surface.read_thread(cid, tid, 50).await {
                    Ok(thread_msgs) if !thread_msgs.is_empty() => {
                        let history = format_thread_history(&thread_msgs, &service.user_map);
                        // Re-hydrated thread history can contain a
                        // persisted `/secret …` message in cleartext;
                        // scrub any captured value back to its marker
                        // before it reaches the model.
                        let history = workforce_common::secret_marker::redact_known_secrets(
                            channel, &history,
                        );
                        effective_message = format!(
                            "[THREAD HISTORY — previous conversation in this thread]\n\
                             This is a new session. The following is the conversation \
                             history from this Slack thread. Use it to maintain \
                             continuity.\n\n{}\n\n[END THREAD HISTORY]\n\n{}",
                            history, effective_message
                        );
                    }
                    Ok(_) => {} // empty thread
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Failed to read thread history for new session"
                        );
                    }
                }
            }
        }
    }

    // Cross-channel continuity: a conversational session opening with no
    // dialogue yet gets a dated digest of the user's own recent memories.
    if let Some(block) = resume_context_block(service, &session, false).await {
        effective_message = format!("{block}\n\n{effective_message}");
    }

    // Same-agent continuity: a 1:1 DM session opening with no dialogue
    // yet gets a digest of this user's other recent sessions.
    if let Some(block) = recent_sessions_digest_block(service, &session, user, channel_id) {
        effective_message = format!("{block}\n\n{effective_message}");
    }

    if let Some(block) = named_secret_context(channel) {
        effective_message = format!("{block}\n\n{effective_message}");
    }

    // An agent that runs programs is told, in the request itself, when to
    // answer with one: guidance a model reads as part of the ask.
    if let Some(hint) = service.programs().as_deref().and_then(|p| p.hint()) {
        effective_message = format!("{hint}\n\n{effective_message}");
    }

    // What the agent has learned about where data lives, for requests
    // like this one (`[know_how] inject = "on"`; `shadow` only records).
    if let Some(block) = know_how_block(
        service,
        &session,
        principal,
        message,
        !session.is_shared_conversation(),
    )
    .await
    {
        effective_message = format!("{block}\n{effective_message}");
    }

    // Answer-ready code context: before the model's first turn, assemble the
    // complete current source for the user's question and prepend it as an
    // authoritative block so the model answers from real code instead of
    // guessing. Gated on `[code_context] enabled` and the assembler's own
    // relevance self-gate (non-code questions assemble nothing → no
    // injection). The raw `message` is the question; the block lands in front
    // of `effective_message`, which `build_task_content` wraps as the model's
    // first user turn. The assembler also yields DETERMINISTIC citations for
    // the files/line-ranges it grounded the answer in — carried out of the
    // turn so the consumer can surface them as clickable chips under the
    // answer (independent of whatever `file:line` prose the model writes).
    let mut code_citations: Vec<workforce_core::code_citation::CodeCitation> = Vec::new();
    // The question as the human asked it, kept before injection. The review
    // passes want these two apart: folded together, the assembled source
    // reaches them labelled "ORIGINAL QUESTION", which is both the largest
    // thing in their prompt and the one thing they most need to recognise as
    // evidence.
    let question_only = effective_message.clone();
    let mut injected_source = String::new();
    let mut injected_block: Option<String> = None;
    if let Some(ctx) = assemble_code_context(service, message, &session).await {
        injected_source = ctx.text.clone();
        // Own content block, outside the trust wrap, through the secret
        // tokenizer — see the non-streaming twin for the full rationale.
        injected_block = Some(
            workforce_common::redact::tokenize_secrets(
                &crate::code_context::build_injected_block(&ctx.text),
                channel,
                workforce_common::secret_marker::SecretSource::Unknown,
            )
            .0,
        );
        code_citations = ctx.citations;
    }
    // Moved into the run task: the deterministic grounding line is streamed
    // as the tail of the answer there — interactive turns only, because a
    // workflow step's output is machine-consumed by later steps and must
    // stay byte-identical.
    let citations_for_answer = code_citations.clone();
    let interactive_turn = !is_automated_source(user);

    let shared = session.is_shared_conversation();
    let (turn_journal, collect) = crate::turn_journal::SessionJournal::open(
        Arc::clone(&service.sessions),
        &mut session,
        shared.then_some(user),
    );
    let archive_base = session.messages.len();

    let store_content = display_content.unwrap_or(message);
    let room_message = desk.and_then(|d| d.turn.message).map(|id| id.to_string());
    session.add_user_message_with_trust(
        store_content,
        external_id.or(room_message.as_deref()),
        attachments.clone(),
        trust_level,
    );
    if let (Some(desk), Some(asked)) = (desk, session.messages.last_mut()) {
        asked.desk = Some(desk.turn.mark());
    }
    // The message this turn answers, for the resume sweep.
    let turn_message_id = session.messages.get(archive_base).map(|m| m.id.clone());

    turn_claim.unless_cancelled(&cancel_token).await?;
    stamp_tool_whitelist(&mut session);
    let turn_tools = crate::turn_loop::tools::turn_tools(service, &session, tool_whitelist).await;
    let turn_prompt = build_enhanced_prompt_with_context(
        service,
        Some(&current_session_id),
        Some(user),
        is_new_session,
        Some(message),
        persona_override,
        prompt_privacy(
            user,
            user,
            prompt_visibility(&session, channel_id),
            RunAuthor::Other,
        ),
        is_workflow_step_session(&session),
        PromptSurface::of(&turn_tools),
    )
    .await;
    note_surfaced_lessons(service, &mut session, &turn_prompt);

    session.set_system_message(&turn_prompt.stable);
    observe_system_prefix(&mut session, &turn_prompt.stable);

    // Update the temporal sidecar AFTER building the prompt. The temporal
    // context block reads this sidecar to compute "time since last
    // interaction"; writing `now` *before* the build would zero the gap
    // (and the builder suppresses gaps < 60s), so the block would never
    // appear and the agent would confabulate how long it's been.
    if let Err(e) = workforce_common::temporal::TemporalSidecar::write(
        &service.workspace,
        user,
        chrono::Utc::now(),
    ) {
        tracing::warn!(error = %e, "Failed to update temporal sidecar");
    }

    // Resolve model: step model override > 4-tier priority (session > preference > agent > default)
    let resolved_model = if let Some(model) = step_model {
        model.to_string()
    } else {
        service.resolve_model(&session, None, Some(message))
    };

    // Reasoning depth for this turn. A spoken turn is judged on how fast
    // it starts talking, so `[llm] voice_thinking_budget_tokens` replaces
    // the client's configured budget for its duration; `None` leaves every
    // provider exactly as configured.
    let turn_thinking_budget = turn_thinking_budget(service, &session);
    let model_aware_config = service.compaction_config_for(&resolved_model).await;

    let config = RunnerConfig {
        agent_id: service.agent_id.clone(),
        system_prompt: turn_prompt.stable.clone(),
        model: Some(resolved_model.clone()),
        max_tokens: Some(
            service
                .active_max_tokens
                .read()
                .unwrap_or(service.effective_llm.max_tokens),
        ),
        max_rounds: resolve_max_rounds(
            step_max_rounds,
            *service.active_max_rounds.read(),
            origin_default_max_rounds(user, &service.effective_llm),
        ),
        team_id: None,
        role: None,
        compaction: model_aware_config.clone(),
        spill: service.current_spill_config(),
        routing: current_routing_config(service),
        tier_registry: service.tier_registry(),
        model_max_tokens: service.effective_llm.model_max_tokens.clone(),
        context_eviction_interval: step_context_eviction_interval,
        context_eviction_keep_last: step_context_eviction_keep_last.unwrap_or(3),
        trace_writer: service.trace_writer.clone(),
        trace_tags: trace_tags(service, channel, Some(message), None),
        permission_writer: service.permission_writer.read().clone(),
        permission_gate: service.permission_gate.read().clone(),
        arguments: service.config.tools.validation.mode,
        tool_loading: service.tool_loading(),
        programs: service.programs(),
        capability_gate: service.capability_gate.read().clone(),
        approval_router: service.approval_router.read().clone(),
        preexecute: workforce_agent::runner::CURRENT_PREEXECUTE
            .try_with(|p| p.clone())
            .ok(),
        journal: Some(turn_journal.clone()),
        collect,
        seeds: None,
        interactive: !is_automated_source(user),
        self_serve,
        compile_gate: service.compile_gate(),
    };

    let runner = runner_on(
        service,
        Arc::clone(&service.llm),
        config.clone(),
        &turn_tools,
    );
    let chain = turn_chain(service, &resolved_model).await;

    // Zone-based compaction: only block when urgently needed, defer otherwise
    let reserved = runner
        .reserved_context_for(&resolved_model)
        .with_prompt_tokens(estimate_tokens(&turn_prompt.turn_context));
    let needs_compaction = session.needs_compaction(&model_aware_config, reserved);
    let needs_urgent = session.needs_urgent_compaction(&model_aware_config, reserved);
    let deferred_compaction = needs_compaction && !needs_urgent;

    // Checkpoint before the longest setup phase: urgent compaction can run
    // for tens of seconds and an interrupted turn must not burn it.
    turn_claim.unless_cancelled(&cancel_token).await?;
    if needs_compaction {
        let memory_writer = crate::notes_tool::memory_writer(service);
        let compaction_engine = runner.compaction_engine_with_writer(memory_writer);

        if needs_urgent {
            // RED ZONE: tokens near hard limit — must compact synchronously
            let _ = progress_tx.send(ProgressEvent::CompactStart).await;
            if let Err(e) = compaction_engine
                .compact_if_needed(&mut session, reserved)
                .await
            {
                tracing::warn!(error = %e, "Compaction failed, continuing with full session");
            }
            let _ = progress_tx.send(ProgressEvent::CompactEnd).await;
        } else {
            // YELLOW ZONE: above threshold but below urgent — fast prune only, defer full compaction
            if let Err(e) = compaction_engine.compact_pruning_only(&mut session) {
                tracing::warn!(error = %e, "Pruning failed, continuing");
            }
            tracing::info!(
                session_id = %current_session_id,
                estimated_tokens = session.estimated_total_tokens(),
                threshold = model_aware_config.trigger_threshold(),
                urgent = model_aware_config.urgent_threshold(),
                "Deferring full compaction to background — proceeding with LLM call"
            );
        }
    }

    // Build conversation history from session (filters empty content,
    // merges consecutive same-role messages, skips tool/system messages)
    let shared_replay = shared_replay(service, &session, user);
    let prior_messages = build_prior_messages(&session.messages, shared_replay.as_ref());
    observe_replay_prefix(&mut session, &prior_messages);
    let session_tools_for_task = Arc::new(crate::tool_loading::session_tools(
        &session.messages,
        service.config.tools.loading.keep_turns,
    ));

    let history_len = prior_messages.len();
    tracing::info!(
        messages = history_len,
        session_total = session.messages.len(),
        session_id = %current_session_id,
        "Loading conversation history"
    );

    // Save session with user message NOW (before spawning task) so that
    // subsequent calls see the latest messages. The spawned task will
    // update it again after the assistant response is ready.
    // Attribute the user message before the pre-run save so a shared session
    // labels the speaker even if the turn is interrupted before it completes.
    if session.is_shared_conversation() {
        session.attribute_messages_from(archive_base, user);
    }
    // Refresh the CAS baseline so the spawned task's post-turn save of this
    // same value is not demoted to an append-merge, which would strand the
    // turn's own metadata (lessons, round-cap continuation) at the stored
    // version.
    service.sessions.save_refreshing(&mut session)?;

    // Open the turn record only once the message it answers is durable: a
    // crash between the two loses the recovery, never the transcript, and
    // the sweep can only re-run what it can see was asked.
    // Only an explicit billing lane is carried: without one, spend falls
    // back to the principal scoped below.
    let billing_identity_for_task = crate::user_config::explicit_billing_identity();
    let turn_token = begin_turn_record(
        &service.sessions,
        &session.id,
        turn_message_id.as_deref(),
        &TurnPrincipal {
            run_as: Some(principal.to_string()),
            bill_to: billing_identity_for_task.clone(),
        },
    );
    let turn_token_for_task = turn_token.clone();

    // Last checkpoint sits before the scheduler messaging context is set:
    // everything after this either completes or is cleaned up by the
    // spawned task. The record opened above must be closed here —
    // otherwise the restart-resume sweep would re-run a turn the user
    // explicitly stopped. (The user message stays saved — the interrupting
    // turn should see it.)
    if cancel_token.is_cancelled() {
        if !crate::shutdown::in_progress() {
            stop_turn_record(&service.sessions, &session.id, &turn_token);
        }
        turn_claim.release().await;
        eyre::bail!("turn cancelled during setup");
    }

    // Set messaging context on scheduler so the scheduler tool can auto-capture it
    if let (Some(ref sched), Some(cid)) = (&service.scheduler, channel_id) {
        sched
            .set_messaging_context(channel, cid.to_string(), thread_id.map(|t| t.to_string()))
            .await;
    }

    let attachment_staging = SessionContext {
        id: Some(current_session_id.clone()),
        ..SessionContext::capture()
    };
    let task_content = build_task_content(
        turn_prompt.task_block(channel).as_deref(),
        injected_block.as_deref(),
        &effective_message,
        &attachments,
        &service.workspace,
        service.resolve_media_strategies(&chain.media_model(&resolved_model)),
        trust_level,
        &attachment_staging,
    )
    .await;
    let sessions = Arc::clone(&service.sessions);
    let sched_for_task = service.scheduler.clone();
    let session_key_for_clear = channel.to_string();
    let task_token = cancel_token.clone();
    let context_tx = progress_tx.clone();
    let context_max_tokens = model_aware_config.max_context_tokens;

    // Cost and telemetry tracking variables for the spawned task
    let cost_tracker = Arc::clone(&service.cost_tracker);
    let telemetry_store = Arc::clone(&service.telemetry_store);
    let telemetry_provider = service.effective_llm.provider.clone();
    let cost_agent_id = service.agent_id.as_str().to_string();
    let prompt_for_telemetry = effective_message.clone();
    let compaction_model = resolved_model;

    // DW-400: capture ticket_id for the spawned closure
    let ticket_id_owned = ticket_id.map(|s| s.to_string());

    // Prepare background compaction variables (only if deferred)
    let bg_llm = if deferred_compaction {
        Some(Arc::clone(&service.llm))
    } else {
        None
    };
    let bg_compaction_config = if deferred_compaction {
        Some(model_aware_config.clone())
    } else {
        None
    };
    let bg_model = if deferred_compaction {
        Some(compaction_model.clone())
    } else {
        None
    };
    let bg_workspace = if deferred_compaction {
        Some(service.workspace.clone())
    } else {
        None
    };
    let bg_memory_store2 = if deferred_compaction {
        service.memory_store.clone()
    } else {
        None
    };
    let bg_agent_id2 = service.agent_id.clone();
    let bg_cost_tracker2 = Arc::clone(&service.cost_tracker);

    // Post-turn extraction (passive memory + DM-only relationship capture)
    // is delegated to `spawn_turn_extraction` from inside the agent task so
    // both interactive paths share one implementation. Carry an `Arc<Self>`
    // and the resolved principal across the spawn (a bare spawn drops the
    // borrow / task-locals).
    let me = Arc::clone(service);
    let rel_principal_for_task = principal.to_string();

    // Capture variables for the lesson pipeline (extraction + reinforcement).
    // Reinforcement always runs — it's a no-op when nothing was injected —
    // so the LLM / store handles are captured unconditionally rather than
    // gated on `lesson_extract`.
    // Shared sessions are excluded from lesson extraction (see the
    // `process_message` site): their turns belong to no single user.
    let ls_extract_enabled =
        service.effective_memory().lesson_extract && !session.is_shared_conversation();
    let ls_llm = Arc::clone(&service.llm);
    let ls_store = service.memory_store.as_ref().map(Arc::clone);
    let ls_agent_id = service.agent_id.clone();
    let ls_extract_model = if ls_extract_enabled {
        lesson_extract_model(service)
    } else {
        None
    };
    let ls_extract_prompt = if ls_extract_enabled {
        service.effective_memory().lesson_extract_prompt.clone()
    } else {
        None
    };
    let ls_cost_tracker = Arc::clone(&service.cost_tracker);
    let ls_sessions = Arc::clone(&service.sessions);

    let archive_base_for_task = archive_base;
    let kh_question = message.to_string();
    let session_key_for_task = channel.to_string();
    let session_id_for_task = current_session_id.clone();
    let user_id_for_task = principal.to_string();
    // Separate owned copy for shared-session attribution: `user_id_for_task`
    // is moved into `CURRENT_USER_ID.scope` and so is unavailable inside the
    // run closure. Attribution records the SENDER, not the run-as principal.
    let user_id_for_attr = user.to_string();
    let platform_user_id_for_task = service.resolve_platform_user_id(principal);
    let channel_id_for_task = channel_id.unwrap_or("").to_string();
    let thread_id_for_task = thread_id.unwrap_or("").to_string();
    let agent_id_for_task = service.agent_id.to_string();
    let workspace_id_for_task = workspace_id.unwrap_or("").to_string();
    let rollover_config_for_task = model_aware_config.clone();
    let rollover_eligible_for_task = !needs_compaction;
    // Carry any active workflow secret policy across the spawn (task-locals
    // don't cross a bare spawn) so the skill sink can resolve declared
    // secrets. Absent for every non-workflow turn — then a no-op.
    let secret_policy_for_task = workforce_common::secret_policy::capture();
    // The turn's environment scope (session `cmd: env use` override, else
    // the global default) expanded into the three task-locals the tool
    // filter + skill/MCP guards read. NOT propagated into `run_isolated`
    // sub-agents, which reset to the default environment.
    let env_scope_for_task = crate::environments::resolve_scope(service, Some(&session));
    let active_environment_for_task = env_scope_for_task.environment.unwrap_or_default();
    let active_skill_instances_for_task = env_scope_for_task.skill_instances;
    let active_mcp_envs_for_task = env_scope_for_task.mcp_envs;
    let voice_channel_for_task = voice_channel.unwrap_or("").to_string();
    let desk_for_task = desk.cloned();
    let desk_scope_for_task = desk.map(|d| workforce_common::channels::desk::DeskScope {
        target: d.target.clone(),
        attended: true,
    });
    let origin_for_task = Origin::of_turn(user, channel, session.is_voice_turn());
    let precurated_for_task = turn_tools.exclusive;
    let principal_for_task = principal.to_string();
    let continuity_for_task = crate::agent_turn::Continuity::of(&session);
    let turn_served = ServedLog::default();
    let served = turn_served.clone();
    let turn_claim_id = turn_claim.claim_id().to_string();
    let handle = tokio::spawn(workforce_agent::runner::with_desk(desk_scope_for_task, CURRENT_BILLING_IDENTITY.scope(
        billing_identity_for_task.unwrap_or_default(),
        CURRENT_VOICE_CHANNEL.scope(
        voice_channel_for_task,
        CURRENT_AGENT_ID.scope(
        agent_id_for_task,
        CURRENT_WORKSPACE_ID.scope(
        workspace_id_for_task,
        CURRENT_CHANNEL_ID.scope(
        channel_id_for_task,
        CURRENT_THREAD_ID.scope(
            thread_id_for_task,
            CURRENT_SESSION_KEY.scope(
                session_key_for_task,
                CURRENT_SESSION_ID.scope(
                    session_id_for_task,
                    CURRENT_USER_ID.scope(user_id_for_task, ACTIVE_ENVIRONMENT.scope(active_environment_for_task, ACTIVE_SKILL_INSTANCES.scope(active_skill_instances_for_task, ACTIVE_MCP_ENVS.scope(active_mcp_envs_for_task, SESSION_TOOLS.scope(session_tools_for_task, workforce_common::secret_policy::scope_if_present(secret_policy_for_task, async move {
                // Hold (and background-renew) the fleet-wide turn claim
                // until every exit path of this task has run.
                let _turn_claim = turn_claim;
                let started_at = chrono::Utc::now();
                // Cloned before the run consumes the runner's sender: the
                // answer streams during the run, so the grounding block (nag
                // mode) or the corrected answer (editor mode) is delivered
                // afterwards as its own follow-up stream segment on this real
                // sender.
                let grounding_progress = progress_tx.clone();
                // Editor mode hides the draft: the runner streams into a
                // filter that drops the answer-text events and relays the
                // agent's tool activity to the real sender, so there is no
                // dead air while the editor runs. The forwarder ends when the
                // runner drops `filtered_tx`, so nothing else may hold it.
                // Installed only when the editor can actually run (source-read
                // tools exist) — otherwise the draft streams live as today.
                let runner_tx = progress_tx;
                // Set the requester's platform id for the cross-channel read
                // gate, around the run so `SessionContext::capture` carries it
                // into the runner's tool-execution spawns.
                let outcome = match CURRENT_PLATFORM_USER_ID
                    .scope(
                        platform_user_id_for_task,
                        with_thinking_budget_override(
                            turn_thinking_budget,
                            PRECURATED_TOOLSET.scope(
                                precurated_for_task,
                                turn_served.clone().scope(Box::pin(chain.run(
                                    &me,
                                    crate::agent_turn::TurnRun {
                                        config,
                                        tools: turn_tools,
                                        task: task_content,
                                        prior: prior_messages,
                                        session: &session,
                                        replay: shared_replay.as_ref(),
                                        progress: Some(runner_tx),
                                        cancel: Some(task_token.clone()),
                                        inject: inject_rx,
                                        principal: &principal_for_task,
                                        oversight: crate::agent_turn::Oversight::Attended(
                                                            crate::mcp_gate::RouteResume::InSession,
                                                        ),
                                        continuity: continuity_for_task,
                                    },
                                ))),
                            ),
                        ),
                    )
                    .await
                {
                    Ok(r) => {
                        // Clear messaging context after agent run
                        if let Some(ref sched) = sched_for_task {
                            sched.clear_messaging_context(&session_key_for_clear).await;
                        }
                        r
                    }
                    Err(e) => {
                        // Clear messaging context on error too
                        if let Some(ref sched) = sched_for_task {
                            sched.clear_messaging_context(&session_key_for_clear).await;
                        }
                        // If cancelled, skip session save — no partial response recorded
                        if task_token.is_cancelled() {
                            if crate::shutdown::in_progress() {
                                tracing::info!(
                                    session_id = %session.id,
                                    "Agent run interrupted by shutdown — session left marked for resume"
                                );
                                return Err(e);
                            }
                            tracing::info!("Agent run cancelled by user");
                            stop_turn_record(&sessions, &session.id, &turn_token_for_task);
                            return Err(e);
                        }
                        tracing::error!(error = %e, "Agent run failed");
                        end_turn_record(&sessions, &session.id, &turn_token_for_task);
                        return Err(e);
                    }
                };

                // If cancelled after completion, skip session save. A
                // shutdown-cancel is NOT that case: the answer is finished
                // and in hand, so fall through to the normal save path —
                // discarding it would make the successor redo the whole
                // turn, side effects included.
                if task_token.is_cancelled() && !crate::shutdown::in_progress() {
                    tracing::info!("Agent completed but was cancelled — skipping session save");
                    stop_turn_record(&sessions, &session.id, &turn_token_for_task);
                    return Err(eyre::eyre!("Cancelled by user"));
                }

                turn_journal
                    .rows()
                    .into_iter()
                    .for_each(|row| session.push_message(row));

                let (mut result, route) = match outcome {
                    crate::agent_turn::ChainOutcome::Served { result, route } => (*result, route),
                    crate::agent_turn::ChainOutcome::Notice { text } => {
                        let saved = save_notice_turn(&me, &mut session, archive_base_for_task, &user_id_for_attr, &text, desk_for_task.as_ref());
                        turn_journal.clear();
                        end_turn_record(&sessions, &session.id, &turn_token_for_task);
                        saved?;
                        return Ok(reply_only(text));
                    }
                };

                {
                    // The answer is finished — everything below only annotates
                    // it. Settle it on screen NOW, so the reviews are never
                    // charged to the reader: they watched the answer arrive and
                    // must not then watch it pretend to still be writing while
                    // a footnote is computed. Whatever streams after this lands
                    // beside the answer instead of inside it.
                    //
                    // Deterministic grounding line — the assembler's own
                    // citations, streamed as the LAST part of the answer so
                    // the reader sees exactly what the answer rests on.
                    // Producer-side and appended to `result.response` too:
                    // screen and response must stay word-identical, or
                    // finalization's reconcile reads the difference as a new
                    // answer and reposts the whole thing (the 07-30 bug
                    // class).
                    if interactive_turn && !citations_for_answer.is_empty() {
                        let line = crate::code_context::grounding_line(&citations_for_answer);
                        result.response.push_str(&line);
                        let _ = grounding_progress.send(ProgressEvent::StreamStart).await;
                        let _ = grounding_progress
                            .send(ProgressEvent::StreamDelta { text: line.clone() })
                            .await;
                        let _ = grounding_progress.send(ProgressEvent::StreamEnd).await;
                    }
                    // No announcement accompanies the reviews below: a check
                    // that fails is a log line, so a claimed check could
                    // never be honestly silent. Silence is uninformative
                    // everywhere — the visible confirmation is the grounding
                    // line above and any real finding beneath.
                    let _ = grounding_progress.send(ProgressEvent::AnswerSettled).await;
                    if post_answer_reviews_eligible(&session) {
                        // The reviews annotate an answer already on screen. If the
                        // reader stops the turn they are not waiting for the
                        // annotation either — so the same token that stops the run
                        // stops these, rather than leaving them burning rounds.
                        let review_cancel = task_token.clone();
                        let answer = result.agent_response().into_owned();
                        let (data_grounding, grounding, commitment) = tokio::join!(
                            data_claim_grounding_block(&me,
                                &effective_message,
                                &answer,
                                &session.id,
                                &user_id_for_attr,
                                &result.tool_invocations,
                                Some(&review_cancel),
                            ),
                            claim_grounding_block(&me,
                                &question_only,
                                &answer,
                                &injected_source,
                                &result.tool_invocations,
                                Some(&review_cancel),
                            ),
                            commitment_caveat_block(&me,
                                &answer,
                                &result.tool_invocations,
                                origin_for_task,
                            ),
                        );
                        let labelled = [
                            (
                                "failed tools",
                                failed_tools_notice(&me, &result.tool_invocations),
                            ),
                            ("data grounding", data_grounding),
                            ("source grounding", grounding.footer()),
                            ("commitments", commitment),
                        ]
                        .into_iter()
                        .filter_map(|(label, block)| block.map(|b| (label, b)))
                        .collect();
                        // ONE annotation message, however many checks fired:
                        // the budgeted segments are joined and streamed as a
                        // single post-settle segment (the consumer posts one
                        // message per segment). Joining the OUTPUT of
                        // `footer_segments` keeps its budget + drop-notice
                        // semantics intact.
                        let segments = footer_segments(labelled);
                        if !segments.is_empty() {
                            let segment = format!("\n\n{}", segments.join("\n\n"));
                            result.response.push_str(&segment);
                            let _ =
                                grounding_progress.send(ProgressEvent::StreamStart).await;
                            let _ = grounding_progress
                                .send(ProgressEvent::StreamDelta { text: segment })
                                .await;
                            let _ = grounding_progress.send(ProgressEvent::StreamEnd).await;
                        }
                    } else {
                        tracing::info!(target: "claim_grounding", agent = %me.agent_id, "skipped: spoken turn — review footers are never spoken");
                    }
                }

                // Persist mid-run follow-ups the runner absorbed — they exist
                // only in the run's in-memory conversation otherwise, and a
                // later turn would be blind to them. System injections (job
                // results) are tool-derived: never promote them into
                // user-authored history.
                for follow_up in result
                    .injected_user_messages
                    .iter()
                    .filter(|f| f.user != "system")
                {
                    session.add_user_message_with_trust(
                        &follow_up.text,
                        follow_up.external_id.as_deref(),
                        None,
                        trust_level,
                    );
                }
                let telemetry_provider = settle_served(&mut session, &result, route, reserved, desk_for_task.as_ref())
                    .map_or(telemetry_provider, str::to_string);
                // Attribute this turn's tool messages to the asker so a shared
                // session's later replay drops them for every other speaker.
                if session.is_shared_conversation() {
                    session.attribute_messages_from(archive_base_for_task, &user_id_for_attr);
                }
                record_round_cap_continuation(&mut session, &result);
                sessions.save_refreshing(&mut session)?;
                turn_journal.clear();
                // The save above may have lost CAS, which discards the
                // metadata it carried; these keys are the turn's own, so
                // they get a narrow write that cannot lose.
                persist_turn_owned_metadata(&sessions, &session);
                // After the transcript, never before: a crash in between
                // leaves a record whose answer is already saved, which the
                // sweep skips — the reverse order would lose the answer
                // and the means to recover it in one step.
                end_turn_record(&sessions, &session.id, &turn_token_for_task);

                for msg in session.messages.get(archive_base_for_task..).unwrap_or_default() {
                    if let Err(e) = sessions.append_to_archive(&session.id, msg) {
                        warn!(error = %e, "Failed to append to session archive");
                    }
                }

                // Infer semantic task type from prompt + tool usage
                let tool_names: Vec<&str> = result
                    .tool_invocations
                    .iter()
                    .map(|t| t.tool_name.as_str())
                    .collect();
                let task_type = infer_task_type(&prompt_for_telemetry, &tool_names);

                // Record cost using the actual model (may differ after failover)
                let cost_model = &result.model;
                let identity_id = crate::user_config::current_billing_identity_id();
                let description = truncate_with_ellipsis(&prompt_for_telemetry, 200);
                cost_tracker.record(CostEntry {
                    task_type: Some(&task_type),
                    ticket_id: ticket_id_owned.as_deref(),
                    identity_id: identity_id.as_deref(),
                    description: Some(&description),
                    served: turn_served.served(),
                    ..CostEntry::new(
                        &cost_agent_id,
                        cost_model,
                        TokenCounts::from(&result.total_usage),
                        origin_for_task,
                    )
                });

                record_task_telemetry_from_result(
                    telemetry_store.as_ref(),
                    &cost_tracker,
                    &cost_agent_id,
                    &telemetry_provider,
                    &result,
                    started_at,
                    Some(&task_type),
                    ticket_id_owned.as_deref(),
                    Some(&description),
                    None,
                );

                // Send context window usage through the progress channel
                let _ = context_tx
                    .send(ProgressEvent::ContextUpdate {
                        estimated_tokens: session.estimated_total_tokens(),
                        context_max: context_max_tokens,
                    })
                    .await;

                tracing::info!(
                    rounds = result.rounds,
                    tools = result.tool_invocations.len(),
                    input_tokens = result.total_usage.input_tokens,
                    output_tokens = result.total_usage.output_tokens,
                    model = %cost_model,
                    "Agent run complete"
                );

                // Fire-and-forget post-turn extraction (passive memory +
                // DM-only relationship capture), shared with
                // `process_message` via `spawn_turn_extraction`.
                spawn_turn_extraction(&me,
                    &session,
                    archive_base_for_task,
                    &effective_message,
                    &result.response,
                    &rel_principal_for_task,
                );
                spawn_know_how(&me,
                    &session,
                    &rel_principal_for_task,
                    &kh_question,
                    &result.tool_invocations,
                )
                .await;

                crate::lessons::run_post_turn(
                    crate::lessons::LessonPipelineConfig {
                        extract_enabled: ls_extract_enabled,
                        llm: &ls_llm,
                        store: ls_store.as_ref(),
                        agent_id: &ls_agent_id,
                        extract_model: ls_extract_model,
                        extract_prompt: ls_extract_prompt,
                        cost_tracker: &ls_cost_tracker,
                        sessions: &ls_sessions,
                    },
                    &mut session,
                    archive_base_for_task,
                    &effective_message,
                    &result.response,
                );

                // Spawn background compaction if it was deferred earlier
                if deferred_compaction {
                    if let (Some(bg_llm), Some(bg_config), Some(bg_workspace)) =
                        (bg_llm, bg_compaction_config, bg_workspace)
                    {
                        let bg_sessions = sessions.clone();
                        let bg_session_id = session.id.clone();
                        let bg_model = bg_model;
                        let bg_reserved = reserved;
                        spawn_in_session_context(async move {
                            // Re-load fresh session (includes the response we just saved)
                            let mut fresh = match bg_sessions.get(&bg_session_id) {
                                Ok(Some(s)) => s,
                                Ok(None) => {
                                    tracing::warn!(
                                        session_id = %bg_session_id,
                                        "Background compaction: session not found"
                                    );
                                    return;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "Background compaction: failed to load session"
                                    );
                                    return;
                                }
                            };
                            let memory_writer: Arc<
                                dyn workforce_agent::compaction::MemoryWriter,
                            > = match bg_memory_store2 {
                                Some(ref store) => Arc::new(
                                    crate::notes_tool::DailyNotesMemoryWriter::with_store(
                                        bg_workspace,
                                        Arc::clone(store),
                                        bg_agent_id2.clone(),
                                    ),
                                ),
                                None => Arc::new(
                                    crate::notes_tool::DailyNotesMemoryWriter::new(bg_workspace),
                                ),
                            };
                            let engine =
                            workforce_agent::compaction::CompactionEngine::with_memory_writer(
                                bg_config,
                                bg_llm,
                                bg_model,
                                memory_writer,
                            )
                            .with_cost_tracker(bg_cost_tracker2, bg_agent_id2.to_string());
                            match engine.compact_if_needed(&mut fresh, bg_reserved).await {
                                Ok(true) => {
                                    if let Err(e) = bg_sessions.save(&fresh) {
                                        tracing::warn!(
                                            error = %e,
                                            "Background compaction: failed to save session"
                                        );
                                    } else {
                                        tracing::info!(
                                            session_id = %bg_session_id,
                                            tokens = fresh.estimated_total_tokens(),
                                            "Background compaction complete"
                                        );
                                    }
                                }
                                Ok(false) => {
                                    tracing::info!(
                                        session_id = %bg_session_id,
                                        "Background compaction: no longer needed"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        session_id = %bg_session_id,
                                        "Background compaction failed"
                                    );
                                }
                            }
                        });
                    }
                }

                // Proactive thread rollover (see `process_message` for the
                // rationale): seal + mint a successor once rollover
                // pressure builds, off the just-saved session.
                if rollover_eligible_for_task
                    && workforce_agent::rollover::rollover_pressure(
                        &session,
                        &rollover_config_for_task,
                        reserved,
                    )
                {
                    workforce_agent::rollover::maybe_roll_over(
                        sessions.clone(),
                        runner.compaction_engine(),
                        &mut session,
                        &rollover_config_for_task,
                        reserved,
                    );
                }

                Ok(result)
            })))))),
        ),
    ),
        ),
    ),
    ),
    ),
    ),
    )));

    Ok(MessageProgress {
        progress_rx,
        handle,
        session_id: current_session_id,
        turn_token,
        turn_claim_id: Some(turn_claim_id),
        cancel_token,
        code_citations,
        served,
    })
}

/// The routes a turn on `spec` runs on: the spec and the agent's live
/// failover list, as its provider chain resolves them.
pub(crate) async fn turn_chain(service: &AgentService, spec: &str) -> crate::agent_turn::TurnChain {
    match &service.failover_provider {
        Some(chain) => crate::agent_turn::TurnChain::group(
            chain.resolve_chain(Some(spec)).await,
            *service.provider_kind.read(),
        ),
        None => pinned_chain(service, spec),
    }
}

/// The one route `spec` alone resolves to.
pub(crate) fn pinned_chain(service: &AgentService, spec: &str) -> crate::agent_turn::TurnChain {
    crate::agent_turn::TurnChain::group(
        vec![workforce_agent::llm::failover::routed_spec(
            &service.tier_registry(),
            spec,
        )],
        *service.provider_kind.read(),
    )
}

/// The loop claim grounding and the reviews run on: the routes of
/// `[claim_grounding] model`, else of the agent's default model.
pub(crate) async fn grounding_loop(
    service: &Arc<AgentService>,
) -> Arc<dyn crate::agent_turn::ToolLoop> {
    crate::agent_turn::tool_loop::tool_loop(
        service,
        service.resolved.snapshot().claim_grounding.model.as_deref(),
    )
    .await
}

/// Whether a turn started now on `model`, else the agent's default, as
/// the identity in scope, has a route to run on: a metered provider in
/// its live chain, or a subscription whose login resolves for the payer.
pub(crate) async fn turn_has_route(service: &AgentService, model: Option<&str>) -> bool {
    let spec = model.map_or_else(
        || service.current_default_model.read().clone(),
        str::to_string,
    );
    turn_chain(service, &spec).await.kinds().iter().any(|kind| {
        kind.subscription_route()
            .is_none_or(|route| service.logins.resolve(route.kind()).is_ok())
    })
}

/// A runner for one turn on `llm`, offered `tools`.
pub(crate) fn runner_on(
    service: &AgentService,
    llm: Arc<dyn workforce_agent::llm::provider::LlmProvider>,
    config: RunnerConfig,
    tools: &TurnTools,
) -> AgentRunner {
    crate::mcp_gate::install_tool_access(
        service,
        AgentRunner::new(
            config,
            llm,
            Arc::clone(&tools.registry),
            Arc::clone(&service.policy),
        ),
    )
}

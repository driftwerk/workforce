//! What a turn extracts after it ends, and as whom.

use workforce_common::subscription::{is_automated_source, personalizes_prompt};

use crate::serve::prompt_visibility;

pub(crate) fn run_as_principal<'a>(run_as: Option<&'a str>, user: &'a str) -> &'a str {
    run_as.filter(|s| !s.is_empty()).unwrap_or(user)
}

/// Turn-shape gates for the recent-sessions digest, layered on top of the
/// session-shape gates in [`crate::session_digest::digest_target_eligible`]:
/// no automated sources (their sessions are user-keyed `Dm`-classified but
/// nobody is reading), no monitor-style non-personalizing turns, and a
/// private delivery destination — belt-and-braces for a DM-classified
/// session delivering to a channel (e.g. a `run_as` job posting publicly).
pub(crate) fn digest_turn_admits(
    session: &workforce_common::session::Session,
    user: &str,
    channel_id: Option<&str>,
) -> bool {
    personalizes_prompt(user)
        && !is_automated_source(user)
        && prompt_visibility(session, channel_id).is_private()
        && !session.read_personal()
}

/// Mark `session`, for good, once a turn of it read a personal connector
/// (the person's bank): a call of `invocations` did, or a tool message it
/// still holds did (a turn that ended before it settled), or the turn's own
/// flag says so (a call through a CLI engine's gate, a session it read
/// back), asked inside the turn's scope. Kept by a patch,
/// so no other writer's save of the session drops it.
pub(crate) fn note_personal_read(
    sessions: &std::sync::Arc<dyn workforce_common::session::SessionStore>,
    session: &mut workforce_common::session::Session,
    invocations: &[workforce_agent::runner::ToolInvocation],
) {
    let read = workforce_agent::personal::read()
        || invocations.iter().any(reads_personal)
        || read_personal(&session.messages);
    if session.read_personal() || !read {
        return;
    }
    session.mark_read_personal();
    let patch = serde_json::Map::from_iter([(
        workforce_common::session::PERSONAL_READ_KEY.to_string(),
        serde_json::Value::Bool(true),
    )]);
    if let Err(e) = sessions.merge_metadata(&session.id, patch, &[]) {
        tracing::warn!(error = %e, session_id = %session.id, "a session that read the person's bank wasn't marked");
    }
}

/// Whether `call` brought a personal connector's data into the turn: one of
/// its tools that answered, or `sessions` reading back a conversation that
/// had.
fn reads_personal(call: &workforce_agent::runner::ToolInvocation) -> bool {
    call.success
        && (workforce_common::connector_binding::is_personal(&call.tool_name)
            || call
                .output
                .get(workforce_common::session::PERSONAL_READ_KEY)
                .is_some_and(|v| v == true))
}

/// Whether `messages` hold what a personal connector (the person's bank)
/// read: nothing of them is remembered or digested.
pub(crate) fn read_personal(messages: &[workforce_common::session::SessionMessage]) -> bool {
    messages.iter().any(|m| {
        matches!(m.role, workforce_common::session::MessageRole::Tool)
            && m.tool_data
                .as_ref()
                .and_then(|d| d.get("name"))
                .and_then(|n| n.as_str())
                .is_some_and(workforce_common::connector_binding::is_personal)
    })
}

/// Decide whether a completed turn should run passive-memory extraction and/or
/// relationship capture. Pure so both interactive entry points
/// ([`AgentService::process_message`] and
/// [`AgentService::process_message_with_progress`]) share one decision and
/// can't drift — relationship capture was once wired into only the former,
/// silently disabling it for every Slack/web turn (which use the latter).
///
/// Relationship capture is restricted to DM turns (the only context that
/// attributes unambiguously to one human) and to a non-empty principal (the
/// canonical identity the per-user view reads back).
///
/// Both passes are suppressed in a shared (multi-human) session: its turns
/// belong to no single user, so extracting them would widen one room's content
/// — a private channel included — into the agent's global memory, recalled for
/// people who were never in the room.
pub(crate) fn plan_turn_extraction(
    mem: &workforce_common::settings::MemoryConfig,
    turn_tokens: u32,
    is_dm: bool,
    is_shared: bool,
    memory_store_present: bool,
    game_sandboxed: bool,
    principal: &str,
) -> (bool, bool) {
    let want_passive = mem.passive
        && turn_tokens >= mem.passive_min_turn_tokens
        && memory_store_present
        && !game_sandboxed
        && !is_shared;
    let want_relationship = mem.relationship
        && turn_tokens >= mem.relationship_min_turn_tokens
        && is_dm
        && !principal.trim().is_empty();
    (want_passive, want_relationship)
}

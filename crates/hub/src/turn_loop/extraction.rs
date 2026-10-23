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
